//! At-most-once dispatch through the authenticated Mods bridge. No native tool-exit inference.
use super::*;

const ACK_TIMEOUT: Duration = Duration::from_secs(5);

enum Sent {
    /// Settled by the acknowledgement of `turn.abort` for exactly this turn.
    Cancel { turn_id: String },
    /// Settled by the main turn that carries exactly this text; the acknowledgement only
    /// reports what `prompt.submit` returned (a drop is a definite failure).
    Send { text: String },
}

struct Dispatched {
    command_id: String,
    sent: Sent,
    deadline: Instant,
}

pub(super) struct Controls {
    receiver: tokio::sync::mpsc::Receiver<ControlRequest>,
    queued: Option<ControlRequest>,
    dispatched: Option<Dispatched>,
}

impl Controls {
    pub(super) fn new(receiver: tokio::sync::mpsc::Receiver<ControlRequest>) -> Self {
        Self { receiver, queued: None, dispatched: None }
    }

    pub(super) fn expire(&mut self, state: &mut SessionData, now: Instant) {
        if self.dispatched.as_ref().is_some_and(|sent| now >= sent.deadline)
            && let Some(sent) = self.dispatched.take()
        {
            let error = match sent.sent {
                Sent::Cancel { .. } => "Claude acknowledgement timed out; do not replay",
                Sent::Send { .. } => {
                    "Claude did not start a turn for the message before its deadline; it may still \
                     run later, do not replay"
                },
            };
            control::uncertain(state, &sent.command_id, error);
        }
        if self.queued.is_none() {
            self.queued = self.receiver.try_recv().ok();
        }
        if self.queued.as_ref().is_some_and(|command| now >= command.queued_at + ACK_TIMEOUT)
            && let Some(command) = self.queued.take()
        {
            control::fail(
                state,
                &command.command_id,
                "Claude did not poll the queued command before its deadline; command was not sent",
            );
        }
    }

    pub(super) fn close(&mut self, state: &mut SessionData) {
        self.dispatched = None;
        self.queued = None;
        while self.receiver.try_recv().is_ok() {}
        let pending: Vec<_> = state
            .commands
            .values()
            .filter(|record| record.receipt.status == CommandStatus::Pending)
            .map(|record| record.receipt.command_id.clone())
            .collect();
        for id in pending {
            control::uncertain(state, &id, "Claude observation stream was lost; do not replay");
        }
    }

    pub(super) fn poll(&mut self, state: &mut SessionData) -> Result<Value> {
        state.storage_ready()?;
        self.expire(state, Instant::now());
        state.storage_ready()?;
        if self.dispatched.is_some() {
            return Ok(json!({}));
        }
        let Some(command) = self.queued.take() else {
            return Ok(json!({}));
        };
        let (sent, value) = match command.operation {
            ControlOperation::Cancel { turn_id } => {
                if state.turn_id.as_ref() != Some(&turn_id) {
                    control::fail(
                        state,
                        &command.command_id,
                        "the requested turn is no longer active",
                    );
                    return Ok(json!({}));
                }
                let value = json!({"command":{"command_id":command.command_id,
                    "operation":"cancel","turn_id":turn_id}});
                (Sent::Cancel { turn_id }, value)
            },
            ControlOperation::Send { text } => {
                if state.state != SessionState::Ready || state.turn_id.is_some() {
                    control::fail(
                        state,
                        &command.command_id,
                        "Claude started another turn first; message was not sent",
                    );
                    return Ok(json!({}));
                }
                let value = json!({"command":{"command_id":command.command_id,
                    "operation":"send","text":text}});
                (Sent::Send { text }, value)
            },
            ControlOperation::Respond { .. } => {
                control::fail(state, &command.command_id, "unsupported Claude control");
                return Ok(json!({}));
            },
        };
        state.command_changed(CommandReceipt {
            command_id: command.command_id.clone(),
            status: CommandStatus::Pending,
            turn_id: match &sent {
                Sent::Cancel { turn_id } => Some(turn_id.clone()),
                Sent::Send { .. } => None,
            },
            error: None,
        });
        if let Err(error) = state.storage_ready() {
            control::fail(
                state,
                &command.command_id,
                "could not persist Claude dispatch; command was not sent",
            );
            return Err(error);
        }
        // The command was durably registered before it was placed on this queue. Taking it
        // consumes the sole delivery attempt even if the HTTP response is lost.
        self.dispatched = Some(Dispatched {
            command_id: command.command_id,
            sent,
            deadline: Instant::now() + ACK_TIMEOUT,
        });
        Ok(value)
    }

    /// A main turn started. It is the delivery of a dispatched message only when it carries
    /// exactly that text; any other turn first makes the message's outcome uncertain.
    pub(super) fn turn_started(
        &mut self,
        state: &mut SessionData,
        turn_id: &str,
        text: Option<&str>,
    ) {
        let Some(Dispatched { sent: Sent::Send { text: expected }, .. }) = &self.dispatched else {
            return;
        };
        let delivered = text == Some(expected.as_str());
        let Some(sent) = self.dispatched.take() else { return };
        if delivered {
            state.command_changed(CommandReceipt {
                command_id: sent.command_id,
                status: CommandStatus::Succeeded,
                turn_id: Some(turn_id.into()),
                error: None,
            });
        } else {
            control::uncertain(
                state,
                &sent.command_id,
                "another turn started before the message; it may still run later, do not replay",
            );
        }
    }

    pub(super) fn acknowledge(&mut self, state: &mut SessionData, value: &Value) -> Result<()> {
        state.storage_ready()?;
        self.expire(state, Instant::now());
        let sent = self
            .dispatched
            .as_ref()
            .filter(|sent| value["command_id"].as_str() == Some(&sent.command_id))
            .ok_or_else(|| rpc(proto::CONFLICT, "no pending Claude acknowledgement"))?;
        let receipt = match &sent.sent {
            Sent::Cancel { turn_id } => {
                if value["turn_id"].as_str() != Some(turn_id) {
                    return Err(rpc(proto::CONFLICT, "Claude acknowledgement identity mismatch"));
                }
                let status = match value["status"].as_str() {
                    Some("succeeded") => CommandStatus::Succeeded,
                    Some("failed") => CommandStatus::Failed,
                    Some("unknown") => CommandStatus::Unknown,
                    _ => return Err(Error::Protocol("invalid Claude acknowledgement status")),
                };
                CommandReceipt {
                    command_id: sent.command_id.clone(),
                    error: match status {
                        CommandStatus::Failed => {
                            Some("the requested turn was no longer active in Claude".into())
                        },
                        CommandStatus::Unknown => {
                            Some("Claude cancellation outcome is uncertain; do not replay".into())
                        },
                        _ => None,
                    },
                    status,
                    turn_id: Some(turn_id.clone()),
                }
            },
            Sent::Send { .. } => match value["status"].as_str() {
                // Submission is not delivery: keep waiting for the turn until the deadline.
                Some("submitted") => return state.storage_ready(),
                Some("dropped") => {
                    let reason: String = value["reason"]
                        .as_str()
                        .unwrap_or("no reason given")
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(160)
                        .collect();
                    CommandReceipt {
                        command_id: sent.command_id.clone(),
                        status: CommandStatus::Failed,
                        turn_id: None,
                        error: Some(format!(
                            "Claude refused the message ({reason}); it was not submitted. \
                             Continue in the native TUI; XaYa does not resend it"
                        )),
                    }
                },
                Some("unknown") => CommandReceipt {
                    command_id: sent.command_id.clone(),
                    status: CommandStatus::Unknown,
                    turn_id: None,
                    error: Some("Claude submit outcome is uncertain; do not replay".into()),
                },
                _ => return Err(Error::Protocol("invalid Claude acknowledgement status")),
            },
        };
        state.command_changed(receipt);
        self.dispatched = None;
        state.storage_ready()
    }
}

impl Bridge {
    pub(super) fn control_request(&mut self, value: &Value, acknowledge: bool) -> Result<Value> {
        let native = identity(value, "sessionId")?;
        if self.invalid
            || self.closed
            || self.stopping.load(Ordering::Acquire)
            || self.expected.as_deref() != Some(&native)
            || self.observed != self.expected
        {
            return Err(rpc(
                proto::CONFLICT,
                "Claude control bridge is closed or identity is unbound",
            ));
        }
        let mut state = lock(&self.state);
        if !state.claude_cancel_ready || !state.state.is_active() || state.verification.is_some() {
            return Err(rpc(proto::CONFLICT, "Claude control bridge is not ready"));
        }
        if acknowledge {
            self.controls.acknowledge(&mut state, value)?;
            Ok(json!({}))
        } else {
            self.controls.poll(&mut state)
        }
    }
}
