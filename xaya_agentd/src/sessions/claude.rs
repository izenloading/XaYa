//! Claude runs as a plain interactive TUI in a PTY host XaYa owns. Identity is chosen by XaYa
//! (`--session-id`); Mods provides structured events and the input gate; process exit and
//! ownership come from the host and the launch marker, never from vendor listings.
use super::*;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use xaya_agent_proto::StartClaudeParams;

use crate::pty_host;
use crate::storage::OwnedLaunch;

const MOD: &str = include_str!("claude_mod.js");
/// Beside the host directory: how a restarted service rejoins this launch's Mods.
const BRIDGE: &str = "bridge.json";
/// The service end of the Mods endpoint; the PTY host relays the loopback port to it.
const MODS_SOCKET: &str = "m.sock";

/// Durable, private record of one launch's Mods endpoint. Without it a launch cannot be rejoined.
#[derive(serde::Serialize, serde::Deserialize)]
struct BridgeRecord {
    version: u32,
    endpoint: String,
    token: String,
    /// The UUID XaYa chose for this launch, so a launch is rejoinable even before Mods has
    /// confirmed it (the snapshot then has to carry exactly this identity).
    #[serde(default)]
    native: Option<String>,
}

/// How a supervised launch begins: a new program in a new host, or joining a live one.
enum Start {
    Launch(NativeIdentity),
    Adopt { native: String, token: String },
}

mod controls;
use controls::Controls;

pub(crate) struct AttachLease {
    state: SharedSession,
    count: Arc<AtomicUsize>,
}

impl Drop for AttachLease {
    fn drop(&mut self) {
        change(&self.state, |data| data.attached = false);
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Sessions {
    pub fn start_claude(&self, runtime: &Runtime, params: StartClaudeParams) -> Result<Session> {
        let directory =
            tempfile::Builder::new().prefix("claude-").tempdir_in(runtime.directory())?;
        self.start_claude_in(params, directory, || Ok(()))
    }

    pub(crate) fn validate_claude_start(params: &StartClaudeParams) -> Result<()> {
        for path in [Some(&params.cwd), Some(&params.executable), params.config_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            if path.len() > 1024 || !Path::new(path).is_absolute() {
                return Err(rpc(
                    proto::INVALID_PARAMS,
                    "Claude paths must be absolute and at most 1024 bytes",
                ));
            }
        }
        let config_ok = match &params.config_dir {
            None => true,
            Some(dir) => Path::new(dir).is_dir() && fs::canonicalize(dir)? == Path::new(dir),
        };
        if !Path::new(&params.cwd).is_dir()
            || !config_ok
            || fs::canonicalize(&params.cwd)? != Path::new(&params.cwd)
        {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "existing canonical cwd and Claude config directory required",
            ));
        }
        let metadata = fs::metadata(&params.executable)?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(rpc(proto::INVALID_PARAMS, "Claude executable is not executable"));
        }
        if params.prompt.trim().is_empty() || params.prompt.len() > 8192 {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "Claude requires an explicit first prompt of 1..8192 bytes",
            ));
        }
        Ok(())
    }

    pub(crate) fn start_claude_in(
        &self,
        params: StartClaudeParams,
        directory: tempfile::TempDir,
        before_persist: impl FnOnce() -> Result<()>,
    ) -> Result<Session> {
        Self::validate_claude_start(&params)?;
        let mut entries = lock(&self.entries);
        self.workspace_available(Path::new(&params.cwd))?;
        if entries.len() >= MAX_SESSIONS {
            return Err(rpc(proto::CONFLICT, "managed session limit reached (16)"));
        }
        let id = directory
            .path()
            .file_name()
            .and_then(|p| p.to_str())
            .ok_or(Error::Protocol("invalid Claude session directory"))?
            .to_owned();
        let native = crate::ownership::new_uuid()?;
        let owned = owned_launch(&directory)?;
        let mut data = SessionData::new(Session {
            id: id.clone(),
            engine: "claude".into(),
            cwd: params.cwd.clone(),
            sandbox: proto::Sandbox::Manual,
            state: SessionState::Starting,
            native_thread_id: None,
            native_job_id: None,
            turn_id: None,
            attached: false,
            backend_pid: None,
            last_error: None,
            verification: None,
        });
        data.persistence = self.store.as_ref().map(|store| Persistence {
            store: Arc::clone(store),
            params: EngineParams::Claude(params.clone()),
            // Durable before the host exists: a crash in between is still recoverable by marker.
            launch: Launch::Owned(owned.clone()),
            handshake: None,
        });
        data.new_epoch()?;
        before_persist()?;
        data.persist();
        data.storage_ready()?;
        let state = Arc::new(Mutex::new(data));
        let identity = NativeIdentity { native, resume: false };
        let entry =
            spawn(params, owned, directory.keep(), Arc::clone(&state), Start::Launch(identity))?;
        entries.insert(id, entry);
        Ok(lock(&state).session.clone())
    }

    /// Continue the same native conversation with `--resume` in a new owned host, only after
    /// every process of the previous launch is confirmed gone. Never replays the first prompt.
    pub(super) fn resume_claude(
        &self,
        runtime: &Runtime,
        entries: &mut BTreeMap<String, ManagedSession>,
        id: &str,
    ) -> Result<Session> {
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        let state = Arc::clone(&entry.state);
        {
            let data = lock(&state);
            if data.state.is_active() {
                if entry.stopping.load(Ordering::Acquire) {
                    return Err(rpc(proto::NOT_READY, "session is stopping"));
                }
                return Ok(data.session.clone());
            }
        }
        if entry.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err(rpc(
                proto::NOT_READY,
                "Claude launch is still supervised; stop it explicitly before resuming",
            ));
        }
        self.workspace_available(Path::new(&lock(&state).cwd))?;
        let directory =
            tempfile::Builder::new().prefix("claude-").tempdir_in(runtime.directory())?;
        let owned = owned_launch(&directory)?;
        let (params, native, previous) = {
            let mut data = lock(&state);
            let native = data
                .native_thread_id
                .clone()
                .ok_or_else(|| rpc(proto::NOT_READY, "session has no native identity to resume"))?;
            let persistence = data
                .persistence
                .as_mut()
                .ok_or(Error::Protocol("session has no persistent identity"))?;
            // Owned launches are scanned by marker: any survivor refuses a second writer.
            crate::storage::can_resume(&persistence.launch)?;
            let EngineParams::Claude(params) = persistence.params.clone() else {
                return Err(Error::Protocol("expected saved Claude parameters"));
            };
            let executable = fs::metadata(&params.executable);
            if !Path::new(&params.cwd).is_dir()
                || params.config_dir.as_ref().is_some_and(|dir| !Path::new(dir).is_dir())
                || !executable.is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            {
                return Err(rpc(
                    proto::NOT_READY,
                    "saved directory, Claude config or executable no longer exists",
                ));
            }
            let previous = match &persistence.launch {
                Launch::Owned(previous) => Some(previous.host_dir.clone()),
                _ => None,
            };
            // Durable before the host exists, exactly as for a first launch.
            persistence.launch = Launch::Owned(owned.clone());
            data.state = SessionState::Starting;
            data.last_error = None;
            data.backend_pid = None;
            data.changed();
            if let Err(error) = data.storage_ready() {
                data.state = SessionState::Failed;
                data.last_error = Some(error.to_string());
                data.changed();
                return Err(error);
            }
            (params, native, previous)
        };
        // The previous launch is confirmed ended: its private guard and host records are spent.
        if let Some(parent) = previous.as_deref().and_then(|dir| Path::new(dir).parent())
            && parent.parent() == Some(runtime.directory())
            && parent.file_name().is_some_and(|name| name.to_string_lossy().starts_with("claude-"))
        {
            let _ = fs::remove_dir_all(parent);
        }
        let identity = NativeIdentity { native, resume: true };
        let entry =
            spawn(params, owned, directory.keep(), Arc::clone(&state), Start::Launch(identity))?;
        entries.insert(id.into(), entry);
        Ok(lock(&state).session.clone())
    }

    pub(crate) fn claim_attach(&self, id: &str) -> Result<(AttachInfo, AttachLease)> {
        let info = self.attach_info(id)?;
        let (dir, child) = {
            let entries = lock(&self.entries);
            let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
            let data = lock(&entry.state);
            let Some(Persistence { launch: Launch::Owned(owned), .. }) = &data.persistence else {
                return Err(rpc(proto::NOT_READY, "attach leases are only supported for Claude"));
            };
            (owned.host_dir.clone(), data.backend_pid)
        };
        // Fresh sample from the host XaYa owns; nothing outside XaYa can attach to it.
        let status = pty_host::status(Path::new(&dir))
            .map_err(|_| rpc(proto::NOT_READY, "Claude PTY host is not reachable"))?;
        if u32::try_from(status.child_pid).ok() != child || status.attached {
            return Err(rpc(proto::NOT_READY, "Claude PTY identity changed; attach was refused"));
        }
        let entries = lock(&self.entries);
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        let mut data = lock(&entry.state);
        data.storage_ready()?;
        if data.engine != "claude"
            || !data.state.is_active()
            || data.attached
            || data.native_thread_id.is_none()
            || entry.stopping.load(Ordering::Acquire)
        {
            return Err(rpc(proto::CONFLICT, "Claude is not available for a native attach"));
        }
        // Share the eight long-lived slots with event subscriptions, leaving control capacity.
        let mut count = self.subscribers.load(Ordering::Acquire);
        loop {
            if count >= 8 {
                return Err(rpc(proto::CONFLICT, "long-lived client limit reached (8)"));
            }
            match self.subscribers.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => count = current,
            }
        }
        data.attached = true;
        data.changed();
        if let Err(error) = data.storage_ready() {
            data.attached = false;
            self.subscribers.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok((info, AttachLease {
            state: Arc::clone(&entry.state),
            count: Arc::clone(&self.subscribers),
        }))
    }
}

fn owned_launch(directory: &tempfile::TempDir) -> Result<OwnedLaunch> {
    Ok(OwnedLaunch {
        host_dir: directory
            .path()
            .join("h")
            .to_str()
            .ok_or(Error::Protocol("non-UTF-8 runtime directory"))?
            .into(),
        marker: crate::ownership::new_marker()?,
        host_pid: None,
        child_pid: None,
        tool_marker: Some(crate::ownership::new_marker()?),
        stop_requested: false,
    })
}

/// Supervise one owned launch. The caller has already persisted `Launch::Owned(owned)`.
/// `directory` is the launch's private directory; it is kept: a still-running program must keep
/// finding its fail-closed gate, and the host directory holds the durable exit record.
fn spawn(
    params: StartClaudeParams,
    owned: OwnedLaunch,
    directory: PathBuf,
    state: SharedSession,
    start: Start,
) -> Result<ManagedSession> {
    let id = lock(&state).id.clone();
    let stopping = Arc::new(AtomicBool::new(false));
    let task_state = Arc::clone(&state);
    let task_stop = Arc::clone(&stopping);
    let executable = params.executable.clone();
    let codex_home = params.config_dir.clone().unwrap_or_default();
    let (controller, receiver) = tokio::sync::mpsc::channel(1);
    let worker = thread::Builder::new().name(format!("Claude {id}")).spawn(move || {
        let result = (|| {
            tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(
                run_claude(&params, start, &owned, &directory, &task_state, &task_stop, receiver),
            )
        })();
        change(&task_state, |data| {
            data.attached = false;
            data.turn_id = None;
            data.native_only_approvals = 0;
            data.claude_cancel_ready = false;
            let pending: Vec<_> = data
                .commands
                .values()
                .filter(|record| record.receipt.status == CommandStatus::Pending)
                .map(|record| record.receipt.command_id.clone())
                .collect();
            for id in pending {
                control::uncertain(
                    data,
                    &id,
                    "Claude bridge stopped before acknowledgement; do not replay",
                );
            }
            match result {
                Ok(exit) => {
                    data.backend_pid = None;
                    // Every owned process is confirmed gone; the checkout is released.
                    data.state = match exit {
                        None | Some(pty_host::Exit { code: Some(0), .. }) => SessionState::Stopped,
                        Some(exit) => {
                            data.last_error = Some(format!(
                                "Claude exited unexpectedly (code {:?}, signal {:?})",
                                exit.code, exit.signal
                            ));
                            SessionState::Failed
                        },
                    };
                    if let Some(saved) = &mut data.persistence {
                        saved.launch = Launch::Ended;
                    }
                },
                Err(ref error) => {
                    data.state = SessionState::Interrupted;
                    data.last_error = Some(
                        format!("Claude ownership requires manual recovery: {error}")
                            .chars()
                            .take(512)
                            .collect(),
                    );
                },
            }
            // Unconfirmed exits retain Owned; an explicit stop can still end them by marker.
        });
    })?;
    Ok(ManagedSession {
        state,
        executable,
        codex_home,
        endpoint: PathBuf::new(),
        stopping,
        worker: Some(worker),
        controller,
    })
}

/// How this launch names the native conversation: a new `--session-id` or a `--resume`.
struct NativeIdentity {
    native: String,
    resume: bool,
}

fn private_write(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn plugin(directory: &Path, endpoint: &str, token: &str) -> Result<PathBuf> {
    let plugin = directory.join("plugin");
    fs::create_dir(&plugin)?;
    fs::create_dir(plugin.join(".claude-plugin"))?;
    fs::create_dir(plugin.join("hooks"))?;
    private_write(
        &plugin.join(".claude-plugin/plugin.json"),
        br#"{"name":"xaya-managed","version":"0.1.0"}"#,
    )?;
    private_write(&plugin.join("hooks/hooks.json"), br#"{"modules":["./register.js"]}"#)?;
    private_write(&plugin.join("hooks/register.js"), MOD.as_bytes())?;
    private_write(
        &plugin.join("hooks/config.js"),
        format!(
            "export const endpoint = {};\nexport const token = {};\n",
            serde_json::to_string(endpoint)?,
            serde_json::to_string(token)?
        )
        .as_bytes(),
    )?;
    Ok(plugin)
}

fn identity(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        })
        .map(str::to_owned)
        .ok_or(Error::Protocol("invalid Claude native identity"))
}

/// Receives the (command ID, decision) of the one panel answer to a waiting permission hook.
type PanelAnswer = tokio::sync::oneshot::Receiver<(String, proto::ApprovalDecision)>;

struct Bridge {
    state: SharedSession,
    expected: Option<String>,
    observed: Option<String>,
    pending: Vec<Observation>,
    sequence: u64,
    invalid: bool,
    closed: bool,
    guarded: bool,
    stopping: Arc<AtomicBool>,
    tools: BTreeMap<String, Value>,
    tasks: std::collections::BTreeSet<String>,
    controls: Controls,
    /// A resumed TUI starts idle: its verified `session.start` means ready for native input.
    resumed: bool,
    /// The last accepted observation (without `late`) and whether it was refused. Mods resend
    /// an event whose reply was cancelled with an aborted turn; an identical copy is a no-op.
    last: Option<(Value, bool)>,
    /// Permission hooks waiting for a panel answer: approval ID -> native tool_use_id.
    waiting: BTreeMap<String, String>,
    /// Joined after a service restart: until Mods sends its snapshot, events and controls are
    /// answered 428 and nothing is admitted.
    adopting: bool,
}

enum Observation {
    Event(Value),
    Permission(String),
}

impl Bridge {
    fn event(&mut self, mut value: Value) -> Result<()> {
        if self.invalid {
            return Err(Error::Protocol("Claude observation stream requires recovery"));
        }
        // A late copy was resent after its hook had already decided (and failed closed).
        let late = value.as_object_mut().and_then(|fields| fields.remove("late"))
            == Some(Value::Bool(true));
        if let Some((last, refused)) = &self.last
            && value["sequence"].as_u64() == Some(self.sequence)
            && *last == value
        {
            return if *refused {
                Err(rpc(proto::CONFLICT, "Claude input gate is closed"))
            } else {
                Ok(())
            };
        }
        let result = self.observe(value.clone(), late);
        match &result {
            Ok(()) => self.last = Some((value, false)),
            Err(Error::Rpc { code: proto::CONFLICT, .. }) => self.last = Some((value, true)),
            Err(_) => self.invalidate(),
        }
        result
    }

    fn invalidate(&mut self) {
        self.invalid = true;
        self.pending.clear();
        self.release_hooks(None);
        self.controls.close(&mut lock(&self.state));
        change(&self.state, |state| {
            state.claude_cancel_ready = false;
            state.state = SessionState::Interrupted;
            state.last_error = Some(
                "Claude Mods identity or observation stream was lost; no state was inferred".into(),
            );
        });
    }

    fn observe(&mut self, value: Value, late: bool) -> Result<()> {
        let sequence =
            value["sequence"].as_u64().ok_or(Error::Protocol("missing Mods sequence"))?;
        let native = identity(&value, "sessionId")?;
        if sequence != self.sequence + 1
            || value["missed"].as_u64() != Some(0)
            || self.observed.as_ref().is_some_and(|id| id != &native)
            || self.expected.as_ref().is_some_and(|id| id != &native)
            || (sequence == 1 && value["type"] != "session.start")
        {
            return Err(Error::Protocol("Claude Mods identity or sequence mismatch"));
        }
        self.sequence = sequence;
        self.observed = Some(native);
        self.activity(&value, late)?;
        if self.expected.is_none() {
            if self.pending.len() >= 128 {
                return Err(Error::Protocol("unbound Mods event limit reached"));
            }
            self.pending.push(Observation::Event(value));
        } else {
            self.apply(&value)?;
        }
        Ok(())
    }

    // Shares the state lock used by workspace freezing. A live Claude is never frozen;
    // only a closed, drained and stopped worker can persist an Ended receipt.
    fn activity(&mut self, value: &Value, late: bool) -> Result<()> {
        let data = &value["data"];
        match value["type"].as_str() {
            Some("session.start") => {
                self.guarded = data["xayaGateVersion"] == 1;
                lock(&self.state).claude_cancel_ready =
                    self.guarded && data["xayaControlVersion"] == 1;
            },
            // Evidence only: its hook already failed closed, so it admits and registers nothing.
            Some("input.start" | "tool.start") if late => (),
            Some("input.start" | "tool.start") => {
                let state = lock(&self.state);
                state.storage_ready()?;
                if self.closed
                    || self.stopping.load(Ordering::Acquire)
                    || state.verification.is_some()
                    || !state.state.is_active()
                {
                    return Err(rpc(proto::CONFLICT, "Claude input gate is closed"));
                }
                if value["type"] == "tool.start" {
                    let id = identity(data, "tool_use_id")?;
                    if self.tools.len() >= 256 || self.tools.contains_key(&id) {
                        return Err(Error::Protocol(
                            "Claude tool identity duplicated or capacity reached",
                        ));
                    }
                    self.tools.insert(id, data.clone());
                }
            },
            Some("tool.complete") => {
                let id = identity(data, "toolUseId")?;
                if !self.tools.contains_key(&id) {
                    return Err(Error::Protocol("Claude completion has no owned tool"));
                }
                let result = &data["result"];
                if !result.is_object()
                    || (result.get("result").is_none()
                        && result.get("deny").is_none()
                        && result["isError"] != true)
                {
                    return Err(Error::Protocol("Claude tool result has no exit evidence"));
                }
                let output = &result["result"];
                if let Some(task) = output.get("backgroundTaskId") {
                    let task = task
                        .as_str()
                        .filter(|id| !id.is_empty() && id.len() <= 256)
                        .ok_or(Error::Protocol("Claude background task identity missing"))?;
                    if self.tasks.len() >= 256 {
                        return Err(Error::Protocol("Claude background task capacity reached"));
                    }
                    self.tasks.insert(task.into());
                }
                if matches!(output["status"].as_str(), Some("async_launched" | "remote_launched")) {
                    // Agent background workers require their own exit evidence; a parent tool
                    // returning is not such evidence. Unknown remote identity fails closed.
                    let agent = identity(output, "agentId")?;
                    if self.tasks.len() >= 256 {
                        return Err(Error::Protocol("Claude background task capacity reached"));
                    }
                    self.tasks.insert(agent);
                }
                if output["interrupted"] == true {
                    return Err(Error::Protocol("Claude interrupted tool exit requires recovery"));
                }
                // TaskStop only signals: even its successful local_bash result does not
                // await shell exit. Keep background ownership until stronger exit evidence
                // is available; never infer it from a turn completion or cancellation ack.
                self.tools.remove(&id);
            },
            _ => (),
        }
        Ok(())
    }

    /// Rejoin a live TUI from its Mods snapshot: the last sequence it assigned, its main turn,
    /// the tools it saw admitted and not finished, and the background tasks it saw launched.
    /// Events up to that sequence are superseded (their admissions already failed closed), so
    /// the stream continues after it. Anything inconsistent leaves the session interrupted.
    fn sync(&mut self, value: &Value) -> Result<()> {
        if !self.adopting || self.invalid {
            return Err(rpc(proto::CONFLICT, "Claude bridge expects no snapshot"));
        }
        crate::faults::crash_point("claude.sync");
        let result = self.adopt(value);
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn adopt(&mut self, value: &Value) -> Result<()> {
        let native = identity(value, "sessionId")?;
        if self.expected.as_deref() != Some(native.as_str()) {
            return Err(Error::Protocol("Claude Mods identity changed across the restart"));
        }
        if value["xayaSyncVersion"] != 1 || value["xayaGateVersion"] != 1 {
            return Err(Error::Protocol("Claude Mods snapshot version is not supported"));
        }
        if value["interrupted"] != false {
            return Err(Error::Protocol("Claude interrupted tool exit requires recovery"));
        }
        let sequence = value["sequence"]
            .as_u64()
            .filter(|sequence| *sequence >= 1)
            .ok_or(Error::Protocol("missing Mods snapshot sequence"))?;
        let turn = match &value["activeTurn"] {
            Value::Null => None,
            _ => Some(identity(value, "activeTurn")?),
        };
        let listed = |field: &str| {
            value[field]
                .as_array()
                .filter(|items| items.len() <= 256)
                .ok_or(Error::Protocol("invalid Claude Mods snapshot"))
        };
        let mut tools = BTreeMap::new();
        for tool in listed("tools")? {
            if !tool.is_object()
                || tools.insert(identity(tool, "tool_use_id")?, tool.clone()).is_some()
            {
                return Err(Error::Protocol("Claude snapshot tool identity duplicated"));
            }
        }
        let mut tasks = std::collections::BTreeSet::new();
        for task in listed("tasks")? {
            let task = task
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or(Error::Protocol("Claude background task identity missing"))?;
            tasks.insert(task.to_owned());
        }
        let mut state = lock(&self.state);
        state.storage_ready()?;
        self.sequence = sequence;
        self.observed = Some(native);
        self.guarded = true;
        self.tools = tools;
        self.tasks = tasks;
        self.last = None;
        self.adopting = false;
        state.claude_cancel_ready = value["xayaControlVersion"] == 1;
        state.state = if turn.is_some() { SessionState::Working } else { SessionState::Ready };
        state.turn_id = turn;
        state.native_thread_id = self.observed.clone();
        state.last_error = None;
        state.changed();
        Ok(())
    }

    fn confirm_exit(&self) -> Result<()> {
        if self.invalid
            || !self.guarded
            || self.observed.is_none()
            || self.expected != self.observed
            || !self.closed
            || !self.tools.is_empty()
            || !self.tasks.is_empty()
        {
            return Err(rpc(
                proto::NOT_READY,
                format!(
                    "Claude worker stopped but tool/task exit is unconfirmed; checkout remains \
                     owned; tools={:?}, tasks={:?}, stream_valid={}",
                    self.tools.keys().collect::<Vec<_>>(),
                    self.tasks,
                    !self.invalid,
                ),
            ));
        }
        Ok(())
    }

    fn bind(&mut self, native: String) -> Result<()> {
        if self.invalid {
            return Err(Error::Protocol("invalid Claude observation stream cannot be bound"));
        }
        if self.observed.as_ref().is_some_and(|id| id != &native) {
            self.invalidate();
            return Err(Error::Protocol("Claude listing and Mods native UUID disagree"));
        }
        self.expected = Some(native);
        if self.observed.is_some() {
            change(&self.state, |state| state.native_thread_id = self.expected.clone());
        }
        for observation in std::mem::take(&mut self.pending) {
            let result = match observation {
                Observation::Event(value) => self.apply(&value),
                Observation::Permission(native) => self.apply_permission(&native),
            };
            if let Err(error) = result {
                self.invalidate();
                return Err(error);
            }
        }
        Ok(())
    }

    /// A PermissionRequest hook arrived. Returns the approval its HTTP request should wait on,
    /// or `None` when it can only be answered in the native TUI.
    fn permission(&mut self, value: &Value) -> Result<Option<(String, PanelAnswer)>> {
        let native = identity(value, "session_id")?;
        if self.invalid {
            return Err(Error::Protocol("Claude observation stream requires recovery"));
        }
        if self.adopting {
            return Ok(None); // Not joined yet: the native prompt decides.
        }
        if self.expected.is_none() {
            if self.pending.len() >= 128 {
                self.invalidate();
                return Err(Error::Protocol("unbound Mods observation limit reached"));
            }
            self.pending.push(Observation::Permission(native));
            return Ok(None);
        }
        if self.expected.as_deref() != Some(native.as_str()) {
            return Ok(None);
        }
        let details = json!({"tool_name": value["tool_name"], "tool_input": value["tool_input"]});
        // Real PermissionRequest payloads carry no tool_use_id. The Mods tool.call wrapper has
        // already registered the asking tool, so bind to the one in-flight main tool of that
        // name that has no waiting hook yet; none or several stays native-only.
        let mut candidates = self.tools.iter().filter(|(id, data)| {
            data["tool"] == value["tool_name"]
                && data.get("agentId").is_none_or(Value::is_null)
                && !self.waiting.values().any(|waiting| waiting == *id)
        });
        let tool = match (candidates.next(), candidates.next()) {
            (Some((id, _)), None) => Some(id.clone()),
            _ => None,
        };
        let mut state = lock(&self.state);
        state.storage_ready()?;
        let turn = state.turn_id.clone();
        let controllable = !self.closed
            && !self.stopping.load(Ordering::Acquire)
            && tool.is_some()
            && value["tool_name"].is_string()
            && serde_json::to_vec(&details)?.len() <= 2048
            && state.approvals.len() < 3
            && turn.is_some()
            && state.state.is_active();
        let (Some(tool), true) = (tool, controllable) else {
            drop(state);
            self.apply_permission(&native)?;
            return Ok(None);
        };
        let id = format!("claude-approval-{}", crate::storage::epoch()?);
        let file = matches!(
            value["tool_name"].as_str(),
            Some("Write" | "Edit" | "MultiEdit" | "NotebookEdit")
        );
        let approval = proto::Approval {
            id: id.clone(),
            kind: if file { proto::ApprovalKind::FileChange } else { proto::ApprovalKind::Command },
            decisions: vec![proto::ApprovalDecision::Accept, proto::ApprovalDecision::Decline],
            method: "PermissionRequest".into(),
            turn_id: turn,
            details,
            controllable: true,
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        state.claude_answers.insert(id.clone(), sender);
        state.approvals.insert(id.clone(), approval.clone());
        state.publish(EventPayload::ApprovalRequested(approval));
        state.state = SessionState::Blocked;
        state.changed();
        self.waiting.insert(id.clone(), tool);
        Ok(Some((id, receiver)))
    }

    /// Releases waiting hooks with no decision: one tool's (`Some`) or all. The native answer
    /// stands; the approval expires and a later panel answer is refused as stale.
    fn release_hooks(&mut self, tool: Option<&str>) {
        let expired: Vec<String> = self
            .waiting
            .iter()
            .filter(|(_, waiting)| tool.is_none_or(|tool| tool == waiting.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        let mut state = lock(&self.state);
        for id in expired {
            self.waiting.remove(&id);
            state.claude_answers.remove(&id);
            state.resolve_approval(&id);
        }
        if state.state == SessionState::Blocked
            && state.approvals.is_empty()
            && state.native_only_approvals == 0
        {
            state.state = SessionState::Working;
            state.changed();
        }
    }

    /// The hook for `id` has returned; its approval is no longer pending in any form.
    fn hook_done(&mut self, id: &str) {
        if let Some(tool) = self.waiting.get(id).cloned() {
            self.release_hooks(Some(&tool));
        }
    }

    fn apply_permission(&self, native: &str) -> Result<()> {
        if self.expected.as_deref() != Some(native) {
            return Ok(());
        }
        let mut state = lock(&self.state);
        state.storage_ready()?;
        if state.turn_id.is_some() && state.state.is_active() {
            state.native_only_approvals = 1;
            state.state = SessionState::Blocked;
            state.publish(EventPayload::NativeOnlyApprovals { count: 1 });
            state.changed();
        }
        Ok(())
    }

    fn apply(&mut self, value: &Value) -> Result<()> {
        let data = &value["data"];
        // The tool ran or was refused (or its turn ended): the native TUI answered first, so a
        // waiting hook is released with no decision. Child agents' tools count too.
        match value["type"].as_str() {
            Some("tool.complete") => {
                if let Some(tool) = data["toolUseId"].as_str() {
                    self.release_hooks(Some(tool));
                }
            },
            Some("turn.complete") if data.get("agentId").is_none_or(Value::is_null) => {
                self.release_hooks(None);
            },
            _ => (),
        }
        // Child-agent events are not boundaries of the main native conversation.
        if data.get("agentId").is_some_and(|id| !id.is_null()) {
            return Ok(());
        }
        let mut state = lock(&self.state);
        state.storage_ready()?;
        match value["type"].as_str() {
            Some("session.start") => {
                state.native_thread_id = self.expected.clone();
                if self.resumed && state.state == SessionState::Starting {
                    state.state = SessionState::Ready;
                }
                state.changed();
            },
            Some("turn.start") => {
                let turn = identity(data, "turnId")?;
                self.controls.turn_started(&mut state, &turn, data["text"].as_str());
                state.turn_id = Some(turn);
                state.state = SessionState::Working;
                state.changed();
            },
            Some("step.chunk") if data["chunk"]["kind"] == "text" => {
                if let (Some(turn), Some(text)) =
                    (data["turnId"].as_str(), data["chunk"]["text"].as_str())
                    && state.turn_id.as_deref() == Some(turn)
                {
                    let mut remaining = text;
                    while !remaining.is_empty() {
                        let mut end = remaining.len().min(8192);
                        while !remaining.is_char_boundary(end) {
                            end -= 1;
                        }
                        state.publish(EventPayload::TextDelta {
                            turn_id: turn.into(),
                            item_id: "claude-text".into(),
                            text: remaining[..end].into(),
                        });
                        remaining = &remaining[end..];
                    }
                }
            },
            Some("turn.complete") => {
                if let Some(turn) = data["turnId"].as_str()
                    && state.turn_id.as_deref() == Some(turn)
                {
                    state.turn_id = None;
                    state.state = SessionState::Ready;
                    state.native_only_approvals = 0;
                    state.publish(EventPayload::NativeOnlyApprovals { count: 0 });
                    state.publish(EventPayload::TurnCompleted {
                        turn_id: turn.into(),
                        status: if data["isAborted"] == true { "interrupted" } else { "completed" }
                            .into(),
                    });
                    state.changed();
                }
            },
            Some("tool.complete") if state.native_only_approvals != 0 => {
                state.native_only_approvals = 0;
                state.state = SessionState::Working;
                state.publish(EventPayload::NativeOnlyApprovals { count: 0 });
                state.changed();
            },
            _ => (), // Unknown vendor events do not crash the service or imply readiness.
        }
        Ok(())
    }
}

async fn http<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    token: &str,
    bridge: &Mutex<Bridge>,
) -> Result<()> {
    // Reading is bounded; only a permission hook may then wait, for the panel's answer.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() >= 8192 {
            return Err(Error::Protocol("Claude HTTP header too large"));
        }
        let mut buffer = [0; 1024];
        let count = tokio::time::timeout_at(deadline, stream.read(&mut buffer))
            .await
            .map_err(|_| Error::Protocol("Claude HTTP read timed out"))??;
        if count == 0 {
            return Err(Error::Protocol("Claude HTTP closed before headers"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| Error::Protocol("invalid HTTP headers"))?;
    let mut lines = headers.split("\r\n");
    let request = lines.next().unwrap_or_default().to_owned();
    let mut authorization = None;
    let mut length = None;
    for line in lines.filter(|s| !s.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(Error::Protocol("malformed HTTP header"))?;
        if name.eq_ignore_ascii_case("X-Xaya-Token") {
            if authorization.is_some() {
                return Err(Error::Protocol("duplicate authorization"));
            }
            authorization = Some(value.trim());
        } else if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(Error::Protocol("duplicate content length"));
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| Error::Protocol("invalid content length"))?,
            );
        } else if name.eq_ignore_ascii_case("Transfer-Encoding") {
            return Err(Error::Protocol("chunked encoding is not supported"));
        }
    }
    let authorized = authorization.is_some_and(|candidate| {
        candidate.len() == token.len()
            && candidate.bytes().zip(token.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    });
    let mut response_body = json!({});
    let status = if !authorized {
        "403 Forbidden"
    } else {
        let length = length
            .filter(|n| *n > 0 && *n <= proto::MAX_FRAME_BYTES)
            .ok_or(Error::Protocol("Claude HTTP body exceeds limit"))?;
        let total = header_end + length;
        while bytes.len() < total {
            let mut buffer = [0; 4096];
            let count = tokio::time::timeout_at(deadline, stream.read(&mut buffer))
                .await
                .map_err(|_| Error::Protocol("Claude HTTP read timed out"))??;
            if count == 0 {
                return Err(Error::Protocol("truncated Claude HTTP body"));
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let value: Value = serde_json::from_slice(&bytes[header_end..total])?;
        if request == "POST /hooks HTTP/1.1" {
            return hook(stream, &value, bridge).await;
        }
        let mut bridge = lock(bridge);
        if request == "POST /sync HTTP/1.1" {
            match bridge.sync(&value) {
                Ok(()) => "200 OK",
                Err(Error::Rpc { code: proto::CONFLICT, .. }) => "409 Conflict",
                Err(error) => return Err(error),
            }
        } else if bridge.adopting
            && matches!(
                request.as_str(),
                "POST /events HTTP/1.1"
                    | "POST /control/poll HTTP/1.1"
                    | "POST /control/ack HTTP/1.1"
            )
        {
            // A restarted service: Mods must send its snapshot before anything else counts.
            "428 Precondition Required"
        } else if request == "POST /events HTTP/1.1" {
            match bridge.event(value) {
                Ok(()) => "200 OK",
                Err(Error::Rpc { code: proto::CONFLICT, .. }) => "409 Conflict",
                Err(error) => return Err(error),
            }
        } else if request == "POST /control/poll HTTP/1.1"
            || request == "POST /control/ack HTTP/1.1"
        {
            match bridge.control_request(&value, request == "POST /control/ack HTTP/1.1") {
                Ok(value) => {
                    response_body = value;
                    "200 OK"
                },
                Err(Error::Rpc { code: proto::CONFLICT, .. }) => "409 Conflict",
                Err(error) => return Err(error),
            }
        } else {
            return Err(Error::Protocol("unknown Claude HTTP route"));
        }
    };
    reply(&mut stream, status, &response_body).await
}

async fn reply(stream: &mut (impl AsyncWrite + Unpin), status: &str, value: &Value) -> Result<()> {
    let body = serde_json::to_string(value)?;
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    Ok(())
}

/// How long a permission hook waits for a panel answer; Claude's own hook timeout is longer.
const HOOK_WAIT: Duration = Duration::from_secs(290);

/// A PermissionRequest waits for exactly one panel answer, or is released with no decision
/// (empty output: the native permission UI decides) when the native TUI answers first, the
/// turn ends, the stream is lost or the wait expires.
async fn hook(
    mut stream: impl AsyncWrite + Unpin,
    value: &Value,
    bridge: &Mutex<Bridge>,
) -> Result<()> {
    let waiting = if value["hook_event_name"] == "PermissionRequest" {
        lock(bridge).permission(value)?
    } else {
        None
    };
    let Some((id, receiver)) = waiting else {
        return reply(&mut stream, "200 OK", &json!({})).await;
    };
    let state = Arc::clone(&lock(bridge).state);
    let answer = timeout(HOOK_WAIT, receiver).await.ok().and_then(|received| received.ok());
    let output = match &answer {
        Some((_, proto::ApprovalDecision::Accept)) => json!({"hookSpecificOutput":{
            "hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}),
        Some(_) => json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest",
            "decision":{"behavior":"deny","message":"Declined in XaYa"}}}),
        None => json!({}),
    };
    let written = reply(&mut stream, "200 OK", &output).await;
    lock(bridge).hook_done(&id);
    if let Some((command_id, _)) = answer {
        // Succeeded: the decision was returned to Claude's hook. If the native TUI answered in
        // the same instant, Claude applies whichever came first; the tool result shows which.
        lock(&state).command_changed(CommandReceipt {
            command_id,
            status: if written.is_ok() { CommandStatus::Succeeded } else { CommandStatus::Unknown },
            turn_id: None,
            error: written.as_ref().err().map(|_| {
                "the answer may not have reached Claude's permission hook; do not replay".into()
            }),
        });
    }
    written
}

fn new_bridge(
    state: &SharedSession,
    stopping: &Arc<AtomicBool>,
    receiver: tokio::sync::mpsc::Receiver<ControlRequest>,
    resumed: bool,
    adopting: bool,
) -> Bridge {
    Bridge {
        state: Arc::clone(state),
        expected: None,
        observed: None,
        pending: Vec::new(),
        sequence: 0,
        invalid: false,
        closed: false,
        guarded: false,
        stopping: Arc::clone(stopping),
        tools: BTreeMap::new(),
        tasks: std::collections::BTreeSet::new(),
        controls: Controls::new(receiver),
        resumed,
        last: None,
        waiting: BTreeMap::new(),
        adopting,
    }
}

/// The service end of the Mods endpoint, in the launch's private directory. A socket left by a
/// crashed service is replaced; anything else at that path is refused.
/// One single-quoted POSIX shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn mods_listener(directory: &Path) -> Result<UnixListener> {
    let path = directory.join(MODS_SOCKET);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
        Ok(_) => return Err(Error::Protocol("unexpected file at the Claude Mods socket path")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn read_bridge(directory: &Path) -> Result<BridgeRecord> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(BRIDGE))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::runtime::user_id()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::Protocol("Claude bridge record is not private"));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 4096), &mut bytes)?;
    let record: BridgeRecord = serde_json::from_slice(&bytes)?;
    if record.version != 1 || record.token.len() != 32 {
        return Err(Error::Protocol("unsupported Claude bridge record"));
    }
    Ok(record)
}

/// Serves Mods HTTP on the launch's Unix socket until dropped (which closes every connection).
struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn serve_mods(listener: UnixListener, token: String, bridge: &Arc<Mutex<Bridge>>) -> Server {
    let bridge = Arc::clone(bridge);
    Server(tokio::spawn(async move {
        // Up to three permission hooks may be waiting; events and controls still get through.
        let permits = Arc::new(tokio::sync::Semaphore::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    if let Ok(permit) = Arc::clone(&permits).try_acquire_owned() {
                        let bridge = Arc::clone(&bridge);
                        let token = token.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            let _ = timeout(HOOK_WAIT + Duration::from_secs(5), http(stream, &token, &bridge)).await;
                        });
                    }
                },
                _ = tasks.join_next(), if !tasks.is_empty() => (),
            }
        }
    }))
}

async fn run_claude(
    params: &StartClaudeParams,
    start: Start,
    owned: &OwnedLaunch,
    directory: &Path,
    state: &SharedSession,
    stopping: &Arc<AtomicBool>,
    receiver: tokio::sync::mpsc::Receiver<ControlRequest>,
) -> Result<Option<pty_host::Exit>> {
    let identity = match start {
        Start::Launch(identity) => identity,
        Start::Adopt { native, token } => {
            // The host still holds the loopback endpoint Mods was configured with; only the
            // service end is new. Nothing is admitted until the snapshot arrives.
            let listener = mods_listener(directory)?;
            let bridge = Arc::new(Mutex::new(new_bridge(state, stopping, receiver, false, true)));
            lock(&bridge).expected = Some(native);
            let _server = serve_mods(listener, token, &bridge);
            return supervise(owned, state, stopping, &bridge, "did not reconnect").await;
        },
    };
    // The host, not the service, holds the loopback port, so it outlives a service crash.
    let relay = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = format!("http://127.0.0.1:{}", relay.local_addr()?.port());
    let token = crate::storage::epoch()?;
    let listener = mods_listener(directory)?;
    private_write(
        &directory.join(BRIDGE),
        &serde_json::to_vec(&BridgeRecord {
            version: 1,
            endpoint: endpoint.clone(),
            token: token.clone(),
            native: Some(identity.native.clone()),
        })?,
    )?;
    let plugin = plugin(directory, &endpoint, &token)?;
    let settings = directory.join("settings.json");
    // Claude runs $CLAUDE_ENV_FILE as a script ahead of every Bash tool command, not for itself
    // or its MCP servers: the tool marker then reaches tool processes only. The tool shell also
    // registers itself, because a platform shell may hide its environment (macOS 27) and so
    // could not show the marker. Registration failures never fail the command.
    let session_start = match &owned.tool_marker {
        Some(tool) => {
            let script = directory.join("tool-env.sh");
            let shells = directory.join(super::TOOL_SHELLS);
            let shells = shells.to_str().ok_or(Error::Protocol("non-UTF-8 directory"))?;
            private_write(
                &script,
                format!(
                    "export {}={tool}\n{{ printf '%s %s\\n' \"$$\" \"$PPID\" >> {}; }} \
                     2>/dev/null || :\n",
                    crate::ownership::TOOL_ENV,
                    shell_quote(shells),
                )
                .as_bytes(),
            )?;
            let script = script.to_str().ok_or(Error::Protocol("non-UTF-8 directory"))?;
            vec![json!({"hooks":[{"type":"command","command":format!(
                "cat {} >> \"$CLAUDE_ENV_FILE\"",
                shell_quote(script)
            )}]})]
        },
        None => Vec::new(),
    };
    private_write(
        &settings,
        &serde_json::to_vec(&json!({
            "hooks":{"PermissionRequest":[{"hooks":[{"type":"http","url":format!("{endpoint}/hooks"),
                "headers":{"X-Xaya-Token":token},"timeout":300}]}],
                "SessionStart":session_start},
            // An interactive TUI otherwise offers "Make auto mode your default permission
            // mode?" once idle, with Yes focused: stray input or a pasted Enter would switch this
            // session to auto and rewrite the user's own defaultMode. A flag-settings defaultMode
            // suppresses that dialog; --permission-mode still decides the starting mode.
            "permissions":{"defaultMode":"manual"}
        }))?,
    )?;
    let bridge =
        Arc::new(Mutex::new(new_bridge(state, stopping, receiver, identity.resume, false)));
    let _server = serve_mods(listener, token, &bridge);
    // XaYa chose the identity; the first Mods event must already carry it.
    lock(&bridge).bind(identity.native.clone())?;
    let mut argv = vec![
        params.executable.clone(),
        if identity.resume { "--resume" } else { "--session-id" }.into(),
        identity.native.clone(),
        "--permission-mode".into(),
        "manual".into(),
        "--plugin-dir".into(),
        plugin.to_str().ok_or(Error::Protocol("non-UTF-8 plugin"))?.into(),
        "--settings".into(),
        settings.to_str().ok_or(Error::Protocol("non-UTF-8 settings"))?.into(),
    ];
    // A resumed conversation waits for native input; the first prompt is never replayed.
    if !identity.resume {
        argv.extend(["--".into(), params.prompt.clone()]);
    }
    let mut env = vec![
        (crate::ownership::OWNER_ENV.to_owned(), owned.marker.clone()),
        ("TERM".to_owned(), std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())),
    ];
    // Only what the caller set: the service's own environment may carry another caller's.
    env.extend(params.config_dir.iter().map(|dir| ("CLAUDE_CONFIG_DIR".to_owned(), dir.clone())));
    let unset: &[&str] = if params.config_dir.is_none() { &["CLAUDE_CONFIG_DIR"] } else { &[] };
    let (executable, dir, cwd) =
        (std::env::current_exe()?, PathBuf::from(&owned.host_dir), PathBuf::from(&params.cwd));
    let target = directory.join(MODS_SOCKET);
    crate::faults::crash_point("launch.before_host");
    let host = tokio::task::spawn_blocking(move || {
        let relay = pty_host::Relay { listener: &relay, target: &target };
        pty_host::spawn_relayed_unset(
            &executable,
            &dir,
            &cwd,
            &argv,
            &env,
            unset,
            pty_host::Size { rows: 24, cols: 80 },
            Some(relay),
        )
    })
    .await
    .map_err(|_| Error::Protocol("PTY host launch panicked"))??;
    crate::faults::crash_point("launch.after_host");
    change(state, |s| {
        s.backend_pid = u32::try_from(host.child_pid).ok();
        if let Some(Persistence { launch: Launch::Owned(saved), .. }) = &mut s.persistence {
            saved.host_pid = Some(host.host_pid);
            saved.child_pid = Some(host.child_pid);
        }
    });
    lock(state).storage_ready()?;
    supervise(owned, state, stopping, &bridge, "did not initialize").await
}

/// Watch one owned launch until it exits or is stopped, then end every owned process.
async fn supervise(
    owned: &OwnedLaunch,
    state: &SharedSession,
    stopping: &Arc<AtomicBool>,
    bridge: &Arc<Mutex<Bridge>>,
    silent: &str,
) -> Result<Option<pty_host::Exit>> {
    let dir = PathBuf::from(&owned.host_dir);
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut unreachable = 0;
    let mut tick = interval(Duration::from_secs(1));
    loop {
        let exit = pty_host::exit_status(&dir)?;
        if stopping.load(Ordering::Acquire) || exit.is_some() {
            // Close the gate first; then end every owned process, including reparented ones.
            lock(bridge).closed = true;
            let saved = owned.clone();
            tokio::task::spawn_blocking(move || stop_owned(&saved))
                .await
                .map_err(|_| Error::Protocol("Claude stop panicked"))??;
            if let Err(error) = lock(bridge).confirm_exit() {
                // Process evidence is authoritative; keep what Mods had not seen finish.
                change(state, |s| {
                    s.last_error = Some(
                        format!("process exit confirmed; Mods ledger was incomplete: {error}")
                            .chars()
                            .take(512)
                            .collect(),
                    )
                });
            }
            return Ok(if stopping.load(Ordering::Acquire) { None } else { exit });
        }
        tick.tick().await;
        lock(bridge).controls.expire(&mut lock(state), Instant::now());
        if lock(bridge).observed.is_none() && Instant::now() >= deadline {
            return Err(Error::Rpc {
                code: proto::ENGINE_ERROR,
                message: format!("Claude Mods {silent}; the owned program may still be running"),
            });
        }
        if pty_host::status(&dir).is_err() && pty_host::exit_status(&dir)?.is_none() {
            unreachable += 1;
            if unreachable >= 3 {
                return Err(Error::Protocol("Claude PTY host became unreachable"));
            }
        } else {
            unreachable = 0;
        }
    }
}

/// Join a Claude launch that outlived the previous service. A host that already exited, or a
/// launch of which nothing runs, is settled by marker; a stop that was asked for is finished; a
/// live host is rejoined through its bridge record, or stays interrupted.
pub(super) fn readopt(
    state: &SharedSession,
    params: StartClaudeParams,
    mut owned: OwnedLaunch,
) -> Option<ManagedSession> {
    let directory = Path::new(&owned.host_dir).parent()?.to_path_buf();
    if owned.stop_requested {
        super::settle(state, &owned, SessionState::Stopped, None);
        return None;
    }
    match super::find_launch(&owned) {
        super::Found::Exited(exit) => {
            let (outcome, error) = if exit.code == Some(0) {
                (SessionState::Stopped, None)
            } else {
                (
                    SessionState::Failed,
                    Some(format!(
                        "Claude ended while the service was down (code {:?}, signal {:?})",
                        exit.code, exit.signal
                    )),
                )
            };
            super::settle(state, &owned, outcome, error);
            None
        },
        super::Found::Gone => {
            let error = if owned.child_pid.is_none() {
                "the service stopped before Claude was launched; start a new session"
            } else {
                "Claude ended while the service was down"
            };
            super::settle(state, &owned, SessionState::Failed, Some(error.into()));
            None
        },
        super::Found::Live(host) => {
            let record = read_bridge(&directory);
            let native = lock(state)
                .native_thread_id
                .clone()
                .or_else(|| record.as_ref().ok().and_then(|record| record.native.clone()));
            let (Some(native), Ok(record)) = (native, record) else {
                change(state, |data| {
                    data.last_error = Some(
                        "this Claude launch cannot be rejoined (no bridge record); stop it \
                         explicitly"
                            .into(),
                    );
                });
                return None;
            };
            super::record_host(state, &mut owned, &host);
            change(state, |data| {
                data.state = SessionState::Starting;
                data.last_error = None;
            });
            let start = Start::Adopt { native, token: record.token };
            match spawn(params, owned, directory, Arc::clone(state), start) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    change(state, |data| {
                        data.state = SessionState::Interrupted;
                        data.backend_pid = None;
                        data.last_error =
                            Some(format!("could not rejoin the running Claude: {error}"));
                    });
                    None
                },
            }
        },
        super::Found::Unconfirmed => None, // Stays interrupted; stop decides by marker.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_round_trips_through_a_posix_shell() {
        for text in ["/tmp/plain", "/tmp/it's here", "a'b''c", "$HOME `x` \\ \"q\""] {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("printf %s {}", shell_quote(text))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), text);
        }
    }
    use tokio::net::{TcpListener, TcpStream};

    fn bridge() -> Bridge {
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        Bridge {
            state: Arc::new(Mutex::new(SessionData::new(Session {
                id: "claude-test".into(),
                engine: "claude".into(),
                cwd: "/tmp".into(),
                sandbox: proto::Sandbox::Manual,
                state: SessionState::Starting,
                native_thread_id: None,
                native_job_id: Some("job".into()),
                turn_id: None,
                attached: false,
                backend_pid: None,
                last_error: None,
                verification: None,
            }))),
            expected: None,
            observed: None,
            pending: Vec::new(),
            sequence: 0,
            invalid: false,
            closed: false,
            guarded: false,
            stopping: Arc::default(),
            tools: BTreeMap::new(),
            tasks: std::collections::BTreeSet::new(),
            controls: Controls::new(receiver),
            resumed: false,
            last: None,
            waiting: BTreeMap::new(),
            adopting: false,
        }
    }

    fn event(sequence: u64, kind: &str, data: Value) -> Value {
        json!({"sessionId":"native","sequence":sequence,"type":kind,"data":data,"missed":0})
    }

    fn bound_bridge() -> Bridge {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        bridge
            .event(event(1, "session.start", json!({"xayaGateVersion":1,"xayaControlVersion":1})))
            .unwrap();
        bridge
    }

    fn adopting_bridge() -> Bridge {
        let mut bridge = bridge();
        bridge.expected = Some("native".into());
        bridge.adopting = true;
        bridge
    }

    fn snapshot(sequence: u64, turn: Value, tools: Value) -> Value {
        json!({"sessionId":"native","sequence":sequence,"activeTurn":turn,"interrupted":false,
            "tools":tools,"tasks":["task-one"],"xayaGateVersion":1,"xayaControlVersion":1,
            "xayaSyncVersion":1})
    }

    #[test]
    fn snapshot_rejoins_turn_and_ledger_then_the_stream_continues_after_it() {
        let mut bridge = adopting_bridge();
        // Nothing counts before the snapshot: a restarted service has no sequence to check.
        assert!(
            bridge
                .permission(&json!({"session_id":"native","tool_name":"Bash"}))
                .unwrap()
                .is_none()
        );
        let tool = json!({"tool":"Bash","tool_use_id":"crossing"});
        bridge.sync(&snapshot(7, json!("turn"), json!([tool]))).unwrap();
        assert!(!bridge.adopting && bridge.guarded);
        assert_eq!(bridge.observed.as_deref(), Some("native"));
        {
            let state = lock(&bridge.state);
            assert_eq!(state.state, SessionState::Working);
            assert_eq!(state.turn_id.as_deref(), Some("turn"));
            assert!(state.claude_cancel_ready);
        }
        assert!(bridge.tasks.contains("task-one"));
        // Only one snapshot per adoption.
        assert!(matches!(
            bridge.sync(&snapshot(7, Value::Null, json!([]))),
            Err(Error::Rpc { code: proto::CONFLICT, .. })
        ));
        assert!(!bridge.invalid);
        assert!(bridge.event(event(7, "turn.complete", json!({"turnId":"turn"}))).is_err());
        let mut bridge = adopting_bridge();
        bridge.sync(&snapshot(7, json!("turn"), json!([tool]))).unwrap();
        let done = json!({"toolUseId":"crossing","result":{"result":{}}});
        bridge.event(event(8, "tool.complete", done)).unwrap();
        bridge.event(event(9, "turn.complete", json!({"turnId":"turn"}))).unwrap();
        assert!(bridge.tools.is_empty());
        assert_eq!(lock(&bridge.state).state, SessionState::Ready);
    }

    #[test]
    fn inconsistent_snapshots_leave_the_session_interrupted() {
        let tool = json!({"tool":"Bash","tool_use_id":"same"});
        let mut cases = vec![
            snapshot(0, Value::Null, json!([])),
            snapshot(3, json!("bad turn id"), json!([])),
            snapshot(3, Value::Null, json!([tool, tool])),
            snapshot(3, Value::Null, json!([{"tool":"Bash"}])),
            snapshot(3, Value::Null, json!(vec![json!({"tool_use_id":"x"}); 257])),
        ];
        let mut other = snapshot(3, Value::Null, json!([]));
        other["sessionId"] = json!("other");
        cases.push(other);
        let mut interrupted = snapshot(3, Value::Null, json!([]));
        interrupted["interrupted"] = json!(true);
        cases.push(interrupted);
        let mut old = snapshot(3, Value::Null, json!([]));
        old["xayaSyncVersion"] = Value::Null;
        cases.push(old);
        for case in cases {
            let mut bridge = adopting_bridge();
            assert!(bridge.sync(&case).is_err(), "{case}");
            assert!(bridge.invalid, "{case}");
            assert_eq!(lock(&bridge.state).state, SessionState::Interrupted, "{case}");
        }
        // A fresh launch's bridge never accepts a snapshot.
        let mut fresh = bound_bridge();
        assert!(fresh.sync(&snapshot(1, Value::Null, json!([]))).is_err());
        assert!(!fresh.invalid);
    }

    #[test]
    fn bridge_record_must_be_private_and_current() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_bridge(directory.path()).is_err(), "no record: not rejoinable");
        let record = br#"{"version":1,"endpoint":"http://127.0.0.1:1","token":"0123456789abcdef0123456789abcdef"}"#;
        private_write(&directory.path().join(BRIDGE), record).unwrap();
        assert_eq!(read_bridge(directory.path()).unwrap().token.len(), 32);
        fs::set_permissions(directory.path().join(BRIDGE), fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(read_bridge(directory.path()).is_err(), "readable by others");
        fs::remove_file(directory.path().join(BRIDGE)).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", directory.path().join(BRIDGE)).unwrap();
        assert!(read_bridge(directory.path()).is_err(), "never follows a link");
        // A stale socket from a crashed service is replaced; any other file is refused.
        let socket = directory.path().join(MODS_SOCKET);
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async { mods_listener(directory.path()).unwrap() });
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, "").unwrap();
        runtime.block_on(async { assert!(mods_listener(directory.path()).is_err()) });
    }

    #[test]
    fn cancellation_ack_checks_identity_never_ends_turn_or_releases_tools() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "turn.start", json!({"turnId":"turn"}))).unwrap();
        bridge
            .event(event(3, "tool.start", json!({"tool":"Bash","tool_use_id":"live-tool"})))
            .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        bridge.controls = Controls::new(receiver);
        queued_cancel(&mut bridge, &sender, "cancel");
        assert!(bridge.control_request(&json!({"sessionId":"wrong"}), false).is_err());
        let polled = bridge.control_request(&json!({"sessionId":"native"}), false).unwrap();
        assert_eq!(polled["command"]["turn_id"], "turn");
        assert_eq!(
            bridge.control_request(&json!({"sessionId":"native"}), false).unwrap(),
            json!({})
        );
        assert!(bridge.control_request(&json!({"sessionId":"native","command_id":"cancel","turn_id":"wrong","status":"succeeded"}), true).is_err());
        assert_eq!(lock(&bridge.state).commands["cancel"].receipt.status, CommandStatus::Pending);
        bridge.control_request(&json!({"sessionId":"native","command_id":"cancel","turn_id":"turn","status":"succeeded"}), true).unwrap();
        assert_eq!(lock(&bridge.state).state, SessionState::Working);
        assert_eq!(lock(&bridge.state).turn_id.as_deref(), Some("turn"));
        bridge.closed = true;
        assert!(
            bridge.confirm_exit().is_err(),
            "cancellation acknowledgement is not tool-exit evidence"
        );
    }

    #[test]
    fn expired_cancellation_ack_cannot_replace_unknown_or_be_delivered_again() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "turn.start", json!({"turnId":"turn"}))).unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        bridge.controls = Controls::new(receiver);
        queued_cancel(&mut bridge, &sender, "cancel");
        bridge.control_request(&json!({"sessionId":"native"}), false).unwrap();
        bridge.controls.expire(&mut lock(&bridge.state), Instant::now() + Duration::from_secs(6));
        assert_eq!(lock(&bridge.state).commands["cancel"].receipt.status, CommandStatus::Unknown);
        assert!(bridge.control_request(&json!({"sessionId":"native","command_id":"cancel","turn_id":"turn","status":"succeeded"}), true).is_err());
        assert_eq!(
            bridge.control_request(&json!({"sessionId":"native"}), false).unwrap(),
            json!({})
        );
        assert_eq!(lock(&bridge.state).commands["cancel"].receipt.status, CommandStatus::Unknown);
    }

    fn queued_cancel(
        bridge: &mut Bridge,
        sender: &tokio::sync::mpsc::Sender<ControlRequest>,
        id: &str,
    ) {
        let operation = ControlOperation::Cancel { turn_id: "turn".into() };
        lock(&bridge.state).commands.insert(id.into(), crate::events::CommandRecord {
            operation: operation.clone(),
            receipt: CommandReceipt {
                command_id: id.into(),
                status: CommandStatus::Pending,
                turn_id: None,
                error: None,
            },
        });
        sender
            .try_send(ControlRequest {
                command_id: id.into(),
                operation,
                queued_at: Instant::now(),
            })
            .unwrap();
    }

    #[test]
    fn queued_cancellation_times_out_and_invalid_bridge_resolves_pending_receipts() {
        for invalidate in [false, true] {
            let mut bridge = bound_bridge();
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            bridge.controls = Controls::new(receiver);
            queued_cancel(&mut bridge, &sender, "queued");
            if invalidate {
                bridge.invalidate();
            } else {
                bridge
                    .controls
                    .expire(&mut lock(&bridge.state), Instant::now() + Duration::from_secs(6));
            }
            assert_ne!(
                lock(&bridge.state).commands["queued"].receipt.status,
                CommandStatus::Pending,
                "unpolled controls must not remain pending forever"
            );
        }
    }

    #[test]
    fn cancellation_expiry_storage_failure_cannot_dispatch_next_command() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("runtime"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        let mut bridge = bound_bridge();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        bridge.controls = Controls::new(receiver);
        {
            let mut state = lock(&bridge.state);
            state.turn_id = Some("turn".into());
            state.persistence = Some(Persistence {
                store,
                params: EngineParams::Claude(StartClaudeParams {
                    cwd: "/tmp".into(),
                    executable: "/bin/false".into(),
                    config_dir: Some("/tmp".into()),
                    prompt: "test".into(),
                }),
                launch: Launch::External,
                handshake: None,
            });
        }
        queued_cancel(&mut bridge, &sender, "first");
        bridge.controls.poll(&mut lock(&bridge.state)).unwrap();
        thread::sleep(Duration::from_millis(5100));
        queued_cancel(&mut bridge, &sender, "second");
        let db = rusqlite::Connection::open(runtime.directory().join("state/agentd.db")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER refuse_commands BEFORE INSERT ON sessions BEGIN SELECT \
             RAISE(FAIL,'fixture'); END;",
        )
        .unwrap();
        // Expire inside poll: its durable Unknown transition must block the next delivery.
        assert!(bridge.controls.poll(&mut lock(&bridge.state)).is_err());
    }

    #[test]
    fn cancellation_dispatch_storage_failure_does_not_strand_pending_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("runtime"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        let mut bridge = bound_bridge();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        bridge.controls = Controls::new(receiver);
        {
            let mut state = lock(&bridge.state);
            state.turn_id = Some("turn".into());
            state.persistence = Some(Persistence {
                store,
                params: EngineParams::Claude(StartClaudeParams {
                    cwd: "/tmp".into(),
                    executable: "/bin/false".into(),
                    config_dir: Some("/tmp".into()),
                    prompt: "test".into(),
                }),
                launch: Launch::External,
                handshake: None,
            });
        }
        queued_cancel(&mut bridge, &sender, "not-sent");
        lock(&bridge.state).persist();
        lock(&bridge.state).storage_ready().unwrap();
        let db = rusqlite::Connection::open(runtime.directory().join("state/agentd.db")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER refuse_commands BEFORE INSERT ON sessions BEGIN SELECT \
             RAISE(FAIL,'fixture'); END;",
        )
        .unwrap();
        assert!(bridge.controls.poll(&mut lock(&bridge.state)).is_err());
        assert_ne!(lock(&bridge.state).commands["not-sent"].receipt.status, CommandStatus::Pending);
    }

    #[test]
    fn task_stop_acknowledgement_is_not_process_exit_evidence() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "tool.start", json!({"tool":"Bash","tool_use_id":"bash"}))).unwrap();
        bridge
            .event(event(
                3,
                "tool.complete",
                json!({"toolUseId":"bash",
            "result":{"result":{"backgroundTaskId":"task"}}}),
            ))
            .unwrap();
        bridge
            .event(event(
                4,
                "tool.start",
                json!({"tool":"TaskStop","tool_use_id":"stop","task_id":"task"}),
            ))
            .unwrap();
        bridge
            .event(event(
                5,
                "tool.complete",
                json!({"toolUseId":"stop",
            "result":{"result":{"task_id":"task","task_type":"local_bash"}}}),
            ))
            .unwrap();
        bridge.closed = true;
        assert!(
            bridge.confirm_exit().is_err(),
            "TaskStop signals a process but does not wait for its exit"
        );
    }

    #[test]
    fn malformed_tool_completion_never_releases_ownership() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "tool.start", json!({"tool":"Bash","tool_use_id":"bash"}))).unwrap();
        assert!(bridge.event(event(3, "tool.complete", json!({"toolUseId":"bash"}))).is_err());
        bridge.closed = true;
        assert!(bridge.confirm_exit().is_err());
    }

    #[test]
    fn observation_without_guard_handshake_cannot_release_checkout() {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        bridge.closed = true;
        assert!(bridge.confirm_exit().is_err());
    }

    #[test]
    fn asynchronous_agents_keep_checkout_owned_after_parent_returns() {
        let mut bridge = bound_bridge();
        bridge
            .event(event(2, "tool.start", json!({"tool":"Agent","tool_use_id":"spawn"})))
            .unwrap();
        bridge
            .event(event(
                3,
                "tool.complete",
                json!({"toolUseId":"spawn",
            "result":{"result":{"status":"async_launched","agentId":"agent-real"}}}),
            ))
            .unwrap();
        bridge.closed = true;
        assert!(
            bridge.confirm_exit().is_err(),
            "an agent can still launch tools after its parent returns"
        );
    }

    #[test]
    fn stop_flag_immediately_rejects_admission_before_cli_poll() {
        let mut bridge = bound_bridge();
        // Session.stop sets this flag before the worker's next one-second listing poll.
        bridge.stopping.store(true, Ordering::Release);
        assert!(bridge.event(event(2, "input.start", json!({}))).is_err());
        assert!(
            bridge
                .event(event(3, "tool.start", json!({"tool":"Write","tool_use_id":"late"})))
                .is_err()
        );
        assert!(!bridge.invalid, "expected gate denials must not discard exit observations");
    }

    #[test]
    fn child_tools_and_background_tasks_outlive_turn_interruptions() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "turn.start", json!({"turnId":"main"}))).unwrap();
        bridge
            .event(event(
                3,
                "tool.start",
                json!({"tool":"Bash","tool_use_id":"child-tool","agentId":"child"}),
            ))
            .unwrap();
        bridge.event(event(4, "turn.complete", json!({"turnId":"main","isAborted":true}))).unwrap();
        bridge.closed = true;
        assert!(bridge.confirm_exit().is_err());
        bridge
            .event(event(
                5,
                "tool.complete",
                json!({"agentId":"child","toolUseId":"child-tool",
            "result":{"result":{"backgroundTaskId":"real-task"}}}),
            ))
            .unwrap();
        assert!(bridge.confirm_exit().is_err());
        bridge.closed = false;
        bridge
            .event(event(
                6,
                "tool.start",
                json!({"tool":"TaskStop","tool_use_id":"stop","task_id":"real-task"}),
            ))
            .unwrap();
        bridge
            .event(event(
                7,
                "tool.complete",
                json!({"toolUseId":"stop",
            "result":{"result":{"task_id":"wrong-task","task_type":"local_bash"}}}),
            ))
            .unwrap();
        bridge.closed = true;
        assert!(bridge.confirm_exit().is_err(), "a mismatched stop must not release the task");
    }

    #[test]
    fn frozen_checkout_rejects_admission_but_accepts_owned_tool_exit() {
        let mut bridge = bound_bridge();
        bridge
            .event(event(2, "tool.start", json!({"tool":"Write","tool_use_id":"owned"})))
            .unwrap();
        lock(&bridge.state).verification = Some("checkpoint".into());
        assert!(bridge.event(event(3, "input.start", json!({}))).is_err());
        assert!(
            bridge
                .event(event(4, "tool.start", json!({"tool":"Write","tool_use_id":"late"})))
                .is_err()
        );
        bridge
            .event(event(5, "tool.complete", json!({"toolUseId":"owned","result":{"result":{}}})))
            .unwrap();
        bridge.closed = true;
        bridge.confirm_exit().unwrap();
    }

    #[test]
    fn resent_observation_is_idempotent_and_keeps_its_first_outcome() {
        let mut bridge = bound_bridge();
        let start = event(2, "turn.start", json!({"turnId":"turn"}));
        bridge.event(start.clone()).unwrap();
        // The first attempt reached the service but its reply was cancelled with the turn.
        let mut resent = start.clone();
        resent["late"] = json!(true);
        bridge.event(resent).unwrap();
        assert_eq!(lock(&bridge.state).turn_id.as_deref(), Some("turn"));
        bridge.event(event(3, "turn.complete", json!({"turnId":"turn"}))).unwrap();
        assert_eq!(lock(&bridge.state).turn_id, None);
        // A refused admission stays refused when resent.
        lock(&bridge.state).verification = Some("checkpoint".into());
        let refused = event(4, "input.start", json!({}));
        assert!(bridge.event(refused.clone()).is_err());
        assert!(bridge.event(refused).is_err());
        assert!(!bridge.invalid);
        lock(&bridge.state).verification = None;
        bridge.event(event(5, "input.start", json!({}))).unwrap();
    }

    #[test]
    fn resent_sequence_with_other_contents_is_a_real_mismatch() {
        let mut bridge = bound_bridge();
        bridge.event(event(2, "turn.start", json!({"turnId":"turn"}))).unwrap();
        assert!(bridge.event(event(2, "turn.start", json!({"turnId":"other"}))).is_err());
        assert!(bridge.invalid);
    }

    #[test]
    fn late_admission_events_are_evidence_only() {
        let mut bridge = bound_bridge();
        lock(&bridge.state).verification = Some("checkpoint".into());
        // Its hook already failed closed; a late copy neither admits nor registers anything.
        let mut tool = event(2, "tool.start", json!({"tool":"Bash","tool_use_id":"denied"}));
        tool["late"] = json!(true);
        bridge.event(tool).unwrap();
        let mut input = event(3, "input.start", json!({}));
        input["late"] = json!(true);
        bridge.event(input).unwrap();
        assert!(bridge.tools.is_empty());
        assert!(!bridge.invalid);
    }

    #[test]
    fn permission_binds_to_its_only_in_flight_tool_or_stays_native_only() {
        let request = json!({"hook_event_name":"PermissionRequest","session_id":"native",
            "tool_name":"Write","tool_input":{"file_path":"a"}});
        let mut bridge = bound_bridge();
        bridge.event(event(2, "turn.start", json!({"turnId":"turn"}))).unwrap();
        // No registered tool of that name: nothing to expire it by, so the TUI decides.
        assert!(bridge.permission(&request).unwrap().is_none());
        assert_eq!(lock(&bridge.state).native_only_approvals, 1);
        bridge.event(event(3, "tool.start", json!({"tool":"Write","tool_use_id":"one"}))).unwrap();
        bridge.event(event(4, "tool.start", json!({"tool":"Bash","tool_use_id":"other"}))).unwrap();
        let (id, _answer) = bridge.permission(&request).unwrap().unwrap();
        assert_eq!(bridge.waiting[&id], "one");
        // A second Write while the first one's hook waits has a single unbound candidate again.
        bridge.event(event(5, "tool.start", json!({"tool":"Write","tool_use_id":"two"}))).unwrap();
        bridge
            .event(event(6, "tool.start", json!({"tool":"Write","tool_use_id":"three"})))
            .unwrap();
        assert!(bridge.permission(&request).unwrap().is_none(), "two candidates are ambiguous");
        // The tool finishing means the native TUI answered: the waiting hook is released.
        bridge
            .event(event(7, "tool.complete", json!({"toolUseId":"one","result":{"result":{}}})))
            .unwrap();
        assert!(bridge.waiting.is_empty());
        assert!(lock(&bridge.state).approvals.is_empty());
        assert!(lock(&bridge.state).claude_answers.is_empty());
    }

    #[test]
    fn native_identity_requires_listing_and_mods_cross_check() {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        assert!(lock(&bridge.state).native_thread_id.is_none());
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        assert_eq!(lock(&bridge.state).native_thread_id.as_deref(), Some("native"));
    }

    #[test]
    fn large_unicode_text_is_delivered_completely_in_bounded_frames() {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        bridge.event(event(2, "turn.start", json!({"turnId":"main"}))).unwrap();
        let subscription = lock(&bridge.state).subscribe(Arc::default()).unwrap();
        let text = "界🙂".repeat(4000);
        bridge
            .event(event(
                3,
                "step.chunk",
                json!({"turnId":"main","chunk":{"kind":"text","text":text}}),
            ))
            .unwrap();
        let mut delivered = String::new();
        for item in subscription.receiver.try_iter() {
            assert!(serde_json::to_vec(&item).unwrap().len() <= proto::MAX_FRAME_BYTES);
            if let EventPayload::TextDelta { text, .. } = item.payload {
                delivered.push_str(&text);
            }
        }
        assert_eq!(delivered.len(), text.len());
        assert_eq!(delivered, text);
    }

    #[test]
    fn child_turn_and_unknown_completion_do_not_end_main_turn() {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        bridge.event(event(2, "turn.start", json!({"turnId":"main"}))).unwrap();
        bridge
            .event(event(3, "turn.complete", json!({"turnId":"main","agentId":"child"})))
            .unwrap();
        bridge.event(event(4, "turn.complete", json!({"turnId":"different"}))).unwrap();
        assert_eq!(lock(&bridge.state).state, SessionState::Working);
        assert_eq!(lock(&bridge.state).turn_id.as_deref(), Some("main"));
    }

    #[test]
    fn missing_sequence_cannot_be_followed_by_inferred_readiness() {
        let mut bridge = bridge();
        bridge.bind("native".into()).unwrap();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        assert!(bridge.event(event(3, "turn.complete", json!({"turnId":"main"}))).is_err());
        assert!(bridge.event(event(2, "turn.start", json!({"turnId":"main"}))).is_err());
        assert_eq!(lock(&bridge.state).state, SessionState::Interrupted);
    }

    #[test]
    fn invalid_unbound_stream_cannot_replay_an_old_ready_state_during_binding() {
        let mut bridge = bridge();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        bridge.event(event(2, "turn.start", json!({"turnId":"main"}))).unwrap();
        bridge.event(event(3, "turn.complete", json!({"turnId":"main"}))).unwrap();
        assert!(bridge.event(event(5, "turn.start", json!({"turnId":"later"}))).is_err());
        assert_eq!(lock(&bridge.state).state, SessionState::Interrupted);
        assert!(bridge.bind("native".into()).is_err());
        assert_eq!(lock(&bridge.state).state, SessionState::Interrupted);
        assert!(lock(&bridge.state).native_thread_id.is_none());
    }

    #[tokio::test]
    async fn initial_permission_is_not_lost_before_listing_identity_is_bound() {
        let mut observer = bridge();
        observer.event(event(1, "session.start", json!({}))).unwrap();
        observer.event(event(2, "turn.start", json!({"turnId":"main"}))).unwrap();
        let bridge = Arc::new(Mutex::new(observer));
        let handler_bridge = Arc::clone(&bridge);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            http(stream, "token", &handler_bridge).await.unwrap();
        });
        let body = json!({"hook_event_name":"PermissionRequest","session_id":"native","tool_name":"Write"}).to_string();
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                format!(
                    "POST /hooks HTTP/1.1\r\nX-Xaya-Token: token\r\nContent-Length: \
                     {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handler.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        let mut bridge = lock(&bridge);
        assert_eq!(lock(&bridge.state).state, SessionState::Starting);
        bridge.bind("native".into()).unwrap();
        assert_eq!(lock(&bridge.state).state, SessionState::Blocked);
        assert_eq!(lock(&bridge.state).native_only_approvals, 1);
    }

    #[test]
    fn unbound_buffer_overflow_invalidates_the_observation_stream() {
        let mut bridge = bridge();
        bridge.event(event(1, "session.start", json!({}))).unwrap();
        for sequence in 2..=128 {
            bridge.event(event(sequence, "unknown.vendor.event", json!({}))).unwrap();
        }
        assert!(bridge.event(event(129, "turn.start", json!({"turnId":"main"}))).is_err());
        assert_eq!(lock(&bridge.state).state, SessionState::Interrupted);
        assert!(bridge.bind("native".into()).is_err());
    }

    async fn request(headers: &str, body: &str) -> (String, u64) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let bridge = Arc::new(Mutex::new(bridge()));
        let observed = Arc::clone(&bridge);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = timeout(Duration::from_secs(1), http(socket, "correct-token", &observed)).await;
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(format!("POST /events HTTP/1.1\r\n{headers}\r\n\r\n{body}").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        server.await.unwrap();
        let sequence = lock(&bridge).sequence;
        (response, sequence)
    }

    #[tokio::test]
    async fn unauthorized_or_ambiguous_http_frames_cannot_publish_events() {
        let body = event(1, "session.start", json!({})).to_string();
        let (response, sequence) =
            request(&format!("X-Xaya-Token: wrong\r\nContent-Length: {}", body.len()), &body).await;
        assert!(response.starts_with("HTTP/1.1 403"));
        assert_eq!(sequence, 0);
        let (response, sequence) = request(
            &format!(
                "X-Xaya-Token: correct-token\r\nContent-Length: {}\r\nContent-Length: {}",
                body.len(),
                body.len()
            ),
            &body,
        )
        .await;
        assert!(!response.contains("200 OK"));
        assert_eq!(sequence, 0);
        let (response, sequence) =
            request("X-Xaya-Token: correct-token\r\nContent-Length: 65537", "{}").await;
        assert!(!response.contains("200 OK"));
        assert_eq!(sequence, 0);
    }

    #[tokio::test]
    async fn authenticated_http_event_is_observed_once() {
        let body = event(1, "session.start", json!({})).to_string();
        let (response, sequence) = request(
            &format!("X-Xaya-Token: correct-token\r\nContent-Length: {}", body.len()),
            &body,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert_eq!(sequence, 1);
    }
}
