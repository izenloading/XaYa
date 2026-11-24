use std::io::{self, BufReader, ErrorKind};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use xaya_agent_proto::{
    self as proto, InitializeParams, InitializeResult, Request, Response, ServerStatus,
    ShutdownResult,
};

use crate::runtime::Runtime;
use crate::wire::{IO_TIMEOUT, read_frame, read_frame_with_limit, response_limit, write_frame};
use crate::{Error, Result};

pub struct Client {
    reader: BufReader<UnixStream>,
    sequence: i64,
    subscription: Option<(String, u64, String)>,
}

impl Client {
    pub fn connect(runtime: &Runtime) -> Result<Self> {
        let mut client =
            Self { reader: BufReader::new(runtime.connect()?), sequence: 0, subscription: None };
        let hello: InitializeResult = client.request(proto::INITIALIZE, InitializeParams {
            protocol_version: proto::PROTOCOL_VERSION,
            client_name: "xaya-agentd-cli".into(),
        })?;
        if hello.protocol_version != proto::PROTOCOL_VERSION
            || !hello.capabilities.iter().any(|capability| capability == proto::STATUS)
        {
            return Err(Error::Protocol("incompatible agent service capabilities"));
        }
        Ok(client)
    }

    pub fn ensure_running(runtime: &Runtime, binary: &Path) -> Result<Self> {
        Self::ensure_running_with_state(runtime, binary, None)
    }

    pub fn ensure_running_with_state(
        runtime: &Runtime,
        binary: &Path,
        state_dir: Option<&Path>,
    ) -> Result<Self> {
        Ok(Self::ensure_running_spawned(runtime, binary, state_dir)?.0)
    }

    /// [`Self::ensure_running_with_state`], also returning the service process this call
    /// started (if it started one and that process still runs), for a long-lived caller to reap.
    pub fn ensure_running_spawned(
        runtime: &Runtime,
        binary: &Path,
        state_dir: Option<&Path>,
    ) -> Result<(Self, Option<std::process::Child>)> {
        let needs_start = match Self::connect(runtime) {
            Ok(client) => return Ok((client, None)),
            Err(Error::Io(error))
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) =>
            {
                true
            },
            Err(Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => false,
            Err(error) => return Err(error),
        };
        let mut child = if needs_start { Some(spawn(runtime, binary, state_dir)?) } else { None };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Self::connect(runtime) {
                Ok(client) => {
                    // A concurrent starter may have won the lock. Reap any exited contender.
                    let running = child.and_then(|mut child| {
                        (!matches!(child.try_wait(), Ok(Some(_)))).then_some(child)
                    });
                    return Ok((client, running));
                },
                Err(Error::Io(error))
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::WouldBlock
                    ) => {},
                Err(error) => {
                    if let Some(child) = child.as_mut() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return Err(error);
                },
            }
            if let Some(child) = child.as_mut() {
                let _ = child.try_wait()?;
            }
            if Instant::now() >= deadline {
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(Error::Protocol(
                    "agent service did not become ready; inspect agentd.log",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        if self.subscription.is_some() {
            return Err(Error::Protocol(
                "use a separate connection for commands after subscribing",
            ));
        }
        self.sequence += 1;
        let request = Request::new(self.sequence, method, params)?;
        write_frame(self.reader.get_mut(), &request)?;
        // Freshness includes a bounded Git snapshot; other RPCs retain their short deadline.
        let response_timeout = if method == proto::ATTACH_CLAIM {
            Duration::from_secs(20)
        } else if matches!(method, proto::SESSION_STOP | proto::SESSION_STOP_IDLE) {
            Duration::from_secs(50)
        } else if matches!(
            method,
            proto::LAND_RECHECK | proto::LAND_RUN | proto::LAND_RECONCILE | proto::LAND_RESUME
        ) {
            Duration::from_secs(150)
        } else if matches!(
            method,
            proto::CHECK_CLEANUP
                | proto::CHECK_GET
                | proto::CHECK_RUN
                | proto::CHECK_PROJECT
                | proto::CHECK_LATEST
                | proto::PROJECT_GET
                | proto::PROJECT_TRUST
        ) {
            Duration::from_secs(70)
        } else {
            IO_TIMEOUT
        };
        let bytes =
            read_frame_with_limit(&mut self.reader, response_timeout, response_limit(method))?;
        match serde_json::from_slice::<Response>(&bytes)? {
            Response::Success(response)
                if response.jsonrpc == "2.0" && response.id == request.id =>
            {
                Ok(serde_json::from_value(response.result)?)
            },
            Response::Failure(response)
                if response.jsonrpc == "2.0" && response.id.as_ref() == Some(&request.id) =>
            {
                Err(Error::Rpc { code: response.error.code, message: response.error.message })
            },
            _ => Err(Error::Protocol("agent response version or request ID did not match")),
        }
    }

    /// The board now; then [`Client::next_board`] returns it again after each change.
    pub fn watch_board(&mut self) -> Result<Vec<proto::ManagedRow>> {
        self.request(proto::BOARD_WATCH, serde_json::Value::Null)
    }

    pub fn next_board(&mut self) -> Result<Vec<proto::ManagedRow>> {
        loop {
            let bytes = read_frame(&mut self.reader, IO_TIMEOUT)?;
            let notification: proto::Notification = serde_json::from_slice(&bytes)?;
            match notification.method.as_str() {
                proto::EVENTS_HEARTBEAT if notification.jsonrpc == "2.0" => continue,
                proto::BOARD_CHANGED if notification.jsonrpc == "2.0" => {
                    return Ok(serde_json::from_value(notification.params)?);
                },
                _ => return Err(Error::Protocol("unexpected board watch notification")),
            }
        }
    }

    pub fn subscribe(&mut self, session_id: &str) -> Result<proto::EventSnapshot> {
        self.subscribe_after(session_id, None)
    }

    /// With `after`, stored events following that cursor arrive first, then live ones; the
    /// snapshot is the current state. A cursor history cannot continue is refused as stale.
    pub fn subscribe_after(
        &mut self,
        session_id: &str,
        after: Option<proto::EventCursor>,
    ) -> Result<proto::EventSnapshot> {
        let snapshot: proto::EventSnapshot =
            self.request(proto::EVENTS_SUBSCRIBE, proto::SubscribeParams {
                session_id: session_id.into(),
                after: after.clone(),
            })?;
        if snapshot.session.id != session_id {
            return Err(Error::Protocol("subscription snapshot belongs to another session"));
        }
        let next = match after {
            Some(cursor) if cursor.epoch != snapshot.epoch || cursor.seq > snapshot.last_seq => {
                return Err(Error::Protocol("subscription does not continue the cursor"));
            },
            Some(cursor) => cursor.seq,
            None => snapshot.last_seq,
        };
        self.subscription = Some((snapshot.epoch.clone(), next, session_id.into()));
        Ok(snapshot)
    }

    pub fn next_event(&mut self) -> Result<proto::SessionEvent> {
        let (epoch, sequence, session_id) =
            self.subscription.as_mut().ok_or(Error::Protocol("subscribe before reading events"))?;
        loop {
            let bytes = read_frame(&mut self.reader, IO_TIMEOUT)?;
            let notification: proto::Notification = serde_json::from_slice(&bytes)?;
            if notification.jsonrpc != "2.0" {
                return Err(Error::Protocol("invalid event protocol version"));
            }
            match notification.method.as_str() {
                proto::EVENTS_HEARTBEAT => continue,
                proto::EVENTS_RESYNC => {
                    return Err(Error::Rpc {
                        code: proto::STALE,
                        message: "event subscriber fell behind; reconnect and take a fresh \
                                  snapshot"
                            .into(),
                    });
                },
                proto::EVENTS_EVENT => {
                    let event: proto::SessionEvent = serde_json::from_value(notification.params)?;
                    if event.epoch != *epoch
                        || event.seq != *sequence + 1
                        || event.session_id != *session_id
                    {
                        return Err(Error::Protocol(
                            "event sequence or epoch changed; reconnect and resynchronize",
                        ));
                    }
                    *sequence = event.seq;
                    return Ok(event);
                },
                _ => return Err(Error::Protocol("unknown subscription notification")),
            }
        }
    }

    pub fn status(&mut self) -> Result<ServerStatus> {
        self.request(proto::STATUS, serde_json::json!({}))
    }

    pub fn shutdown(&mut self) -> Result<ShutdownResult> {
        self.request(proto::SHUTDOWN, serde_json::json!({}))
    }
}

fn spawn(
    runtime: &Runtime,
    binary: &Path,
    state_dir: Option<&Path>,
) -> Result<std::process::Child> {
    let log = runtime.private_file("agentd.log", true)?;
    let mut command = Command::new(binary);
    if let Some(state_dir) = state_dir {
        command.arg("--state-dir").arg(state_dir);
    }
    command
        .arg("--runtime-dir")
        .arg(runtime.directory())
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // SAFETY: setsid is async-signal-safe and this closure does not allocate or acquire locks.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn subscriber() -> (Client, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        (
            Client {
                reader: BufReader::new(reader),
                sequence: 1,
                subscription: Some(("epoch".into(), 10, "session".into())),
            },
            writer,
        )
    }

    fn event(epoch: &str, seq: u64) -> proto::Notification {
        proto::Notification {
            jsonrpc: "2.0".into(),
            method: proto::EVENTS_EVENT.into(),
            params: json!({"session_id":"session","epoch":epoch,"seq":seq,"kind":"turn_completed",
                "data":{"turn_id":"turn","status":"completed"}}),
        }
    }

    #[test]
    fn subscriber_validates_epoch_and_sequence_and_skips_heartbeats() {
        for notification in [event("epoch", 12), event("changed-epoch", 11)] {
            let (mut client, mut writer) = subscriber();
            write_frame(&mut writer, &notification).unwrap();
            assert!(client.next_event().is_err());
        }
        let (mut client, mut writer) = subscriber();
        write_frame(&mut writer, &proto::Notification {
            jsonrpc: "2.0".into(),
            method: proto::EVENTS_HEARTBEAT.into(),
            params: json!(null),
        })
        .unwrap();
        write_frame(&mut writer, &event("epoch", 11)).unwrap();
        assert_eq!(client.next_event().unwrap().seq, 11);
        assert!(client.status().is_err());
    }

    #[test]
    fn subscriber_reports_overflow_as_a_resync_error() {
        let (mut client, mut writer) = subscriber();
        write_frame(&mut writer, &proto::Notification {
            jsonrpc: "2.0".into(),
            method: proto::EVENTS_RESYNC.into(),
            params: json!(null),
        })
        .unwrap();
        assert!(matches!(client.next_event(), Err(Error::Rpc { code: proto::STALE, .. })));
    }
}
