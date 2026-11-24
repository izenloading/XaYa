//! One dedicated app-server per managed session, with a private Unix WebSocket for its TUI.
//! The backend runs in an XaYa-owned PTY host and listens on a private socket this service
//! connects to, so a crash of the service does not end a running turn: the next service adopts
//! the backend on a new connection. Backend ownership is independent of the attached terminal.
//! All vendor frames stay in this crate.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fs, io};

use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, accept_async_with_config, client_async_with_config};
use xaya_agent_proto::{
    self as proto, AttachInfo, CommandReceipt, CommandStatus, ControlOperation, EventPayload,
    Session, SessionState, StartSessionParams,
};

use crate::events::{SessionData, SharedSession, Subscription};
use crate::runtime::Runtime;
use crate::{Error, Result, pty_host};

mod claude;
mod control;
use crate::storage::{EngineParams, Launch, OwnedLaunch, Persistence, Store};
use control::ControlRequest;

// Vendor metadata is routinely much larger than XaYa control frames. Bound both transports
// independently; do not apply the control socket's 64 KiB limit to native Codex traffic.
const ENGINE_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_SESSIONS: usize = 16;
const MAX_PENDING: usize = 256;
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

type Terminal = WebSocketStream<UnixStream>;
type BackendSocket = WebSocketStream<UnixStream>;

/// A held board-watch slot; released when the watch ends.
pub(crate) struct BoardWatch(Arc<AtomicUsize>);

impl Drop for BoardWatch {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    // State is only copied or assigned while locked; no user code runs under these locks.
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rpc(code: i32, message: impl Into<String>) -> Error {
    Error::Rpc { code, message: message.into() }
}

struct ManagedSession {
    state: SharedSession,
    executable: String,
    codex_home: String,
    endpoint: PathBuf,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    controller: tokio::sync::mpsc::Sender<ControlRequest>,
}

impl ManagedSession {
    fn discardable(&self) -> bool {
        let data = lock(&self.state);
        !data.state.is_active()
            && data.verification.is_none()
            && data
                .persistence
                .as_ref()
                .is_none_or(|saved| crate::storage::can_resume(&saved.launch).is_ok())
    }
}

#[derive(Default)]
pub struct Sessions {
    entries: Mutex<BTreeMap<String, ManagedSession>>,
    subscribers: Arc<AtomicUsize>,
    store: Option<Arc<Store>>,
    workspace_leases: Mutex<BTreeMap<PathBuf, String>>,
}

pub(crate) struct WorkspaceLease<'a> {
    sessions: &'a Sessions,
    root: PathBuf,
    id: String,
}

impl Drop for WorkspaceLease<'_> {
    fn drop(&mut self) {
        let entries = lock(&self.sessions.entries);
        let mut leases = lock(&self.sessions.workspace_leases);
        leases.remove(&self.root);
        for entry in entries.values() {
            let mut data = lock(&entry.state);
            if data.verification.as_deref() == Some(&self.id) {
                data.verification = None;
                data.changed();
            }
        }
    }
}

impl Sessions {
    pub(crate) fn open(store: Arc<Store>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for mut record in store.load()? {
            if !(record.session.id.starts_with("codex-")
                || record.session.id.starts_with("claude-"))
                || record.session.id.len() > 64
                || !record
                    .session
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err(Error::Protocol("invalid persisted session identity"));
            }
            if record.session.state.is_active() {
                record.session.state = SessionState::Interrupted;
                record.session.last_error =
                    Some("service restarted; explicitly attach or resume to continue".into());
            }
            record.session.backend_pid = None;
            record.session.attached = false;
            record.session.turn_id = None;
            record.session.verification = None;
            for command in record.commands.values_mut() {
                if command.receipt.status == CommandStatus::Pending {
                    command.receipt.status = CommandStatus::Unknown;
                    command.receipt.error = Some(
                        "service restarted before acknowledgement; do not replay automatically"
                            .into(),
                    );
                }
            }
            let id = record.session.id.clone();
            let executable = record.params.executable().to_owned();
            let codex_home = record.params.home().to_owned();
            let mut data = SessionData::new(record.session);
            data.commands = record.commands;
            data.persistence = Some(Persistence {
                store: Arc::clone(&store),
                params: record.params,
                launch: record.launch,
                handshake: record.handshake,
            });
            // The stored stream continues: clients resume after their last seen event.
            match store.event_head(&id)? {
                Some(head) => data.restore_stream(head),
                None => data.new_epoch()?,
            }
            data.persist();
            data.storage_ready()?;
            let (controller, receiver) = tokio::sync::mpsc::channel(1);
            drop(receiver);
            entries.insert(id, ManagedSession {
                state: Arc::new(Mutex::new(data)),
                executable,
                codex_home,
                endpoint: PathBuf::new(),
                stopping: Arc::default(),
                worker: None,
                controller,
            });
        }
        Ok(Self {
            entries: Mutex::new(entries),
            subscribers: Arc::default(),
            store: Some(store),
            workspace_leases: Mutex::default(),
        })
    }

    pub(crate) fn freeze_workspace(
        &self,
        session_id: &str,
        root: &Path,
        id: &str,
    ) -> Result<WorkspaceLease<'_>> {
        self.freeze(Some(session_id), root, id, None)
    }

    /// A landing passes its own preview, which is exempt from the unresolved-landing gate.
    /// Without a session, the lease serves recovery after the origin session was evicted.
    pub(crate) fn freeze_workspace_for_landing(
        &self,
        session_id: Option<&str>,
        root: &Path,
        id: &str,
        preview_id: &str,
    ) -> Result<WorkspaceLease<'_>> {
        self.freeze(session_id, root, id, Some(preview_id))
    }

    fn freeze(
        &self,
        session_id: Option<&str>,
        root: &Path,
        id: &str,
        landing: Option<&str>,
    ) -> Result<WorkspaceLease<'_>> {
        self.check_previous_verifications(root)?;
        self.check_unresolved_landings(root, landing)?;
        let entries = lock(&self.entries);
        if session_id.is_some_and(|session_id| !entries.contains_key(session_id)) {
            return Err(rpc(proto::NOT_FOUND, "unknown session"));
        }
        let mut leases = lock(&self.workspace_leases);
        if leases.keys().any(|path| path.starts_with(root) || root.starts_with(path)) {
            return Err(rpc(proto::CONFLICT, "workspace already has a checkpoint in progress"));
        }
        let mut states = Vec::new();
        for entry in entries.values() {
            let data = lock(&entry.state);
            let cwd = Path::new(&data.cwd).canonicalize()?;
            if cwd.starts_with(root) {
                states.push(data);
            }
        }
        for data in &states {
            if data.engine == "claude"
                && (data.state.is_active()
                    || !data
                        .persistence
                        .as_ref()
                        .is_some_and(|saved| matches!(saved.launch, Launch::Ended)))
            {
                return Err(rpc(
                    proto::CONFLICT,
                    "Claude native input and tool exit cannot be frozen while it runs; stop it \
                     first. Verification is refused",
                ));
            }
            data.storage_ready()?;
            if data.state.is_active()
                && (data.state != SessionState::Ready
                    || data.turn_id.is_some()
                    || data.pending_requests != 0
                    || data.commands.values().any(|c| c.receipt.status == CommandStatus::Pending))
            {
                return Err(rpc(
                    proto::CONFLICT,
                    "workspace has an active turn or pending request",
                ));
            }
            if !data.state.is_active()
                && let Some(saved) = &data.persistence
            {
                crate::storage::can_resume(&saved.launch)?;
            }
        }
        for data in &mut states {
            data.verification = Some(id.into());
            data.changed();
        }
        // The gate is in-memory; restart interrupts the checkpoint, never adopts it.
        leases.insert(root.to_owned(), id.into());
        let lease = WorkspaceLease { sessions: self, root: root.to_owned(), id: id.into() };
        let error = states.iter().find_map(|data| data.storage_ready().err());
        drop(states);
        drop(leases);
        drop(entries);
        if let Some(error) = error {
            drop(lease);
            return Err(error);
        }
        Ok(lease)
    }

    fn workspace_available(&self, cwd: &Path) -> Result<()> {
        let cwd = cwd.canonicalize()?;
        self.check_previous_verifications(&cwd)?;
        self.check_unresolved_landings(&cwd, None)?;
        if lock(&self.workspace_leases).keys().any(|root| cwd.starts_with(root)) {
            return Err(rpc(proto::CONFLICT, "workspace checkpoint in progress"));
        }
        Ok(())
    }

    fn check_previous_verifications(&self, cwd: &Path) -> Result<()> {
        if let Some(store) = &self.store {
            for record in store.load_checks()? {
                if record.check.status == proto::CheckStatus::Interrupted {
                    let root = Path::new(&record.check.root);
                    if root.starts_with(cwd) || cwd.starts_with(root) {
                        crate::storage::can_resume(&record.launch)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// A crash may leave a checked-out target between its old version and the candidate.
    /// Until a reconcile or resume settles that, no managed writer may start there; the
    /// durable record outlives the in-memory lease and a restart.
    fn check_unresolved_landings(&self, cwd: &Path, except: Option<&str>) -> Result<()> {
        if let Some(store) = &self.store {
            for preview in store.load_previews()? {
                let unresolved = preview.status != proto::LandStatus::Landed
                    && preview.landing_outcome == Some(proto::LandOutcome::Unknown)
                    && except != Some(preview.id.as_str());
                if let Some(target) = preview.target_worktree.as_deref().filter(|_| unresolved) {
                    let target = Path::new(target);
                    if target.starts_with(cwd) || cwd.starts_with(target) {
                        return Err(rpc(
                            proto::CONFLICT,
                            format!(
                                "target checkout has an unresolved landing {}; run `xaya land \
                                 reconcile` or `xaya land resume` first",
                                preview.id
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn resume(&self, runtime: &Runtime, id: &str) -> Result<Session> {
        let mut entries = lock(&self.entries);
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        if lock(&entry.state).engine == "claude" {
            return self.resume_claude(runtime, &mut entries, id);
        }
        self.workspace_available(Path::new(&lock(&entry.state).cwd))?;
        let state = Arc::clone(&entry.state);
        let params = {
            let mut data = lock(&state);
            if data.state.is_active() {
                return Ok(data.session.clone());
            }
            if data.native_thread_id.is_none() {
                return Err(rpc(proto::NOT_READY, "session has no native thread to resume"));
            }
            let persistence = data
                .persistence
                .as_mut()
                .ok_or(Error::Protocol("session has no persistent identity"))?;
            crate::storage::can_resume(&persistence.launch)?;
            let EngineParams::Codex(params) = persistence.params.clone() else {
                return Err(Error::Protocol("expected saved Codex parameters"));
            };
            if !Path::new(&params.cwd).is_dir() || !Path::new(&params.executable).is_file() {
                return Err(rpc(
                    proto::NOT_READY,
                    "saved directory or engine executable no longer exists",
                ));
            }
            persistence.launch = Launch::Prepared;
            data.state = SessionState::Starting;
            data.last_error = None;
            data.changed();
            if let Err(error) = data.storage_ready() {
                data.state = SessionState::Failed;
                data.last_error = Some(error.to_string());
                data.changed();
                return Err(error);
            }
            params
        };
        let root =
            tempfile::Builder::new().prefix("codex-").tempdir_in(runtime.directory())?.keep();
        let launched = new_owned(&root)
            .and_then(|owned| Self::launch(params, Arc::clone(&state), root.clone(), owned, None));
        let entry = match launched {
            Ok(entry) => entry,
            Err(error) => {
                remove_launch_root(&root);
                change(&state, |data| {
                    data.state = SessionState::Failed;
                    data.last_error = Some(error.to_string());
                    if let Some(saved) = &mut data.persistence {
                        saved.launch = Launch::Ended;
                    }
                });
                return Err(error);
            },
        };
        entries.insert(id.into(), entry);
        Ok(lock(&state).session.clone())
    }

    /// Claim one of the eight long-lived client slots for a board watch.
    pub(crate) fn watch_board(&self) -> Result<BoardWatch> {
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
                Ok(_) => return Ok(BoardWatch(Arc::clone(&self.subscribers))),
                Err(current) => count = current,
            }
        }
    }

    pub fn board(&self) -> Vec<proto::ManagedRow> {
        lock(&self.entries)
            .values()
            .map(|entry| {
                let data = lock(&entry.state);
                proto::ManagedRow {
                    session: data.session.clone(),
                    approval_count: data.approvals.len() + data.native_only_approvals as usize,
                }
            })
            .collect()
    }

    pub(crate) fn retired(&self, id: &str) -> Result<bool> {
        self.store.as_ref().map_or(Ok(false), |store| store.retired(id))
    }

    pub fn start(&self, runtime: &Runtime, params: StartSessionParams) -> Result<Session> {
        let directory =
            tempfile::Builder::new().prefix("codex-").tempdir_in(runtime.directory())?;
        self.start_codex_in(params, directory, || Ok(()))
    }

    pub(crate) fn validate_codex_start(params: &StartSessionParams) -> Result<()> {
        if params
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty() || prompt.len() > 8192)
        {
            return Err(rpc(proto::INVALID_PARAMS, "the first message must contain 1..8192 bytes"));
        }
        if params.sandbox == proto::Sandbox::Manual {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "manual permissions belong to Claude; Codex requires an explicit sandbox mode",
            ));
        }
        if [&params.cwd, &params.executable, &params.codex_home]
            .iter()
            .any(|path| path.len() > 1024)
        {
            return Err(rpc(proto::INVALID_PARAMS, "session paths exceed 1024 bytes"));
        }
        let cwd = Path::new(&params.cwd);
        let executable = Path::new(&params.executable);
        if !cwd.is_absolute()
            || !cwd.is_dir()
            || !executable.is_absolute()
            || !Path::new(&params.codex_home).is_absolute()
        {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "existing absolute directory and executable required",
            ));
        }
        let metadata = fs::metadata(executable)?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(rpc(proto::INVALID_PARAMS, "engine path must be executable"));
        }
        Ok(())
    }

    pub(crate) fn start_codex_in(
        &self,
        params: StartSessionParams,
        directory: tempfile::TempDir,
        before_persist: impl FnOnce() -> Result<()>,
    ) -> Result<Session> {
        Self::validate_codex_start(&params)?;
        let mut entries = lock(&self.entries);
        self.workspace_available(Path::new(&params.cwd))?;
        // Keep stopped records until capacity is needed. Active sessions are never evicted.
        if entries.len() >= MAX_SESSIONS {
            let expired: Vec<_> = entries
                .iter()
                .filter(|(_, entry)| {
                    entry.discardable() && lock(&entry.state).state != SessionState::Interrupted
                })
                .map(|(id, _)| id.clone())
                .collect();
            if let Some(store) = &self.store {
                store.retire(&expired)?;
            }
            entries.retain(|id, _| !expired.contains(id));
        }
        if entries.len() >= MAX_SESSIONS {
            return Err(rpc(proto::CONFLICT, "managed session limit reached (16)"));
        }
        let id = directory
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Protocol("invalid session directory"))?
            .to_owned();
        let state = Arc::new(Mutex::new(SessionData::new(Session {
            id: id.clone(),
            engine: "codex".into(),
            cwd: params.cwd.clone(),
            sandbox: params.sandbox,
            state: SessionState::Starting,
            native_thread_id: None,
            native_job_id: None,
            turn_id: None,
            attached: false,
            backend_pid: None,
            last_error: None,
            verification: None,
        })));
        {
            let mut data = lock(&state);
            data.persistence = self.store.as_ref().map(|store| Persistence {
                store: Arc::clone(store),
                params: EngineParams::Codex(params.clone()),
                launch: Launch::Prepared,
                handshake: None,
            });
            data.new_epoch()?;
            before_persist()?;
            data.persist();
            data.storage_ready()?;
        }
        let root = directory.keep();
        let launched = new_owned(&root)
            .and_then(|owned| Self::launch(params, Arc::clone(&state), root.clone(), owned, None));
        let entry = match launched {
            Ok(entry) => entry,
            Err(error) => {
                remove_launch_root(&root);
                if let Some(store) = &self.store {
                    store.remove(std::slice::from_ref(&id))?;
                }
                return Err(error);
            },
        };
        let session = lock(&state).clone();
        entries.insert(id, entry);
        Ok(session)
    }

    /// Supervises one hosted Codex backend. For a fresh launch the backend is started in an
    /// XaYa-owned PTY host first; with `adopt`, a backend that survived a crash of the previous
    /// service is joined on a new connection. Either way the backend lives in its own host, so
    /// only an orderly exit of this worker ends it, never the death of this process.
    fn launch(
        params: StartSessionParams,
        state: SharedSession,
        root: PathBuf,
        owned: OwnedLaunch,
        adopt: Option<Value>,
    ) -> Result<ManagedSession> {
        let id = lock(&state).id.clone();
        let endpoint = root.join("tui.sock");
        // A crashed service may have left its endpoint behind; it is never a live listener.
        if fs::symlink_metadata(&endpoint).is_ok_and(|meta| meta.file_type().is_socket()) {
            fs::remove_file(&endpoint)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(&endpoint)?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        if adopt.is_none() {
            // Durable before the host exists: a crash in between is still recoverable by marker.
            change(&state, |data| {
                if let Some(persistence) = &mut data.persistence {
                    persistence.launch = Launch::Owned(owned.clone());
                }
            });
            lock(&state).persist();
            lock(&state).storage_ready()?;
        }
        let (controller, commands) = tokio::sync::mpsc::channel(32);
        let stopping = Arc::new(AtomicBool::new(false));
        let task_state = Arc::clone(&state);
        let task_stop = Arc::clone(&stopping);
        let executable = params.executable.clone();
        let codex_home = params.codex_home.clone();
        // Without a native TUI to start it, the service itself initializes the backend and
        // starts the main thread for a first message.
        let headless = params.prompt.clone().map(|prompt| (params.cwd.clone(), prompt));
        let handshake = adopt.clone().or_else(|| headless.as_ref().map(|_| default_handshake()));
        let worker = thread::Builder::new().name(format!("agent {id}")).spawn(move || {
            let outcome = (|| {
                let executor =
                    tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                executor.block_on(async {
                    let listener = UnixListener::from_std(listener)?;
                    if adopt.is_none() {
                        crate::faults::crash_point("launch.before_host");
                        let host = spawn_host(&params, &owned).await?;
                        crate::faults::crash_point("launch.after_host");
                        change(&task_state, |state| {
                            state.backend_pid = u32::try_from(host.child_pid).ok();
                            if let Some(Persistence { launch: Launch::Owned(saved), .. }) =
                                &mut state.persistence
                            {
                                saved.host_pid = Some(host.host_pid);
                                saved.child_pid = Some(host.child_pid);
                            }
                        });
                        lock(&task_state).persist();
                        lock(&task_state).storage_ready()?;
                    }
                    let backend = connect_backend(&owned, &task_stop).await?;
                    run(backend, listener, &task_state, &task_stop, commands, handshake, headless)
                        .await
                })
            })();
            // Every exit of this worker ends the hosted backend and its tools; only the death of
            // the service itself leaves them running for the next service to adopt.
            let ended = stop_owned(&owned);
            if ended.is_ok() {
                remove_launch_root(&root);
            }
            let mut session = lock(&task_state);
            session.attached = false;
            session.turn_id = None;
            session.backend_pid = None;
            if let Some(persistence) = &mut session.persistence
                && ended.is_ok()
            {
                persistence.launch = Launch::Ended;
            }
            if task_stop.load(Ordering::Acquire) {
                session.state = SessionState::Stopped;
            } else {
                session.state = SessionState::Failed;
                session.last_error = Some(match outcome {
                    Ok(()) => "Codex backend disconnected".into(),
                    Err(error) => error.to_string().chars().take(512).collect(),
                });
            }
            if let Err(error) = ended {
                session.last_error = Some(
                    format!("{error}; the checkout stays owned until stop confirms the exit")
                        .chars()
                        .take(512)
                        .collect(),
                );
            }
            let approvals: Vec<_> = session.approvals.keys().cloned().collect();
            for id in approvals {
                session.resolve_approval(&id);
            }
            if session.native_only_approvals > 0 {
                session.native_only_approvals = 0;
                session.publish(EventPayload::NativeOnlyApprovals { count: 0 });
            }
            let pending: Vec<_> = session
                .commands
                .values()
                .filter(|record| record.receipt.status == CommandStatus::Pending)
                .map(|record| record.receipt.command_id.clone())
                .collect();
            for id in pending {
                control::uncertain(
                    &mut session,
                    &id,
                    "backend stopped before acknowledgement; do not replay automatically",
                );
            }
            session.changed();
        })?;
        Ok(ManagedSession {
            state,
            executable,
            codex_home,
            endpoint,
            stopping,
            worker: Some(worker),
            controller,
        })
    }

    /// Joins Codex backends and Claude TUIs that outlived the previous service. A launch whose
    /// host already exited is settled once its owned processes are confirmed gone; one whose
    /// host cannot be confirmed stays interrupted and blocks a second writer.
    pub(crate) fn readopt(&self) {
        let claude: Vec<_> = lock(&self.entries)
            .iter()
            .filter_map(|(id, entry)| {
                let data = lock(&entry.state);
                let saved = data.persistence.as_ref()?;
                match (&saved.params, &saved.launch, data.engine.as_str()) {
                    (EngineParams::Claude(params), Launch::Owned(owned), "claude") => {
                        Some((id.clone(), Arc::clone(&entry.state), params.clone(), owned.clone()))
                    },
                    _ => None,
                }
            })
            .collect();
        for (id, state, params, owned) in claude {
            if let Some(entry) = claude::readopt(&state, params, owned) {
                lock(&self.entries).insert(id, entry);
            }
        }
        let candidates: Vec<_> = lock(&self.entries)
            .iter()
            .filter_map(|(id, entry)| {
                let data = lock(&entry.state);
                let saved = data.persistence.as_ref()?;
                match (&saved.params, &saved.launch, data.engine.as_str()) {
                    (EngineParams::Codex(params), Launch::Owned(owned), "codex") => Some((
                        id.clone(),
                        Arc::clone(&entry.state),
                        params.clone(),
                        owned.clone(),
                        saved.handshake.clone(),
                    )),
                    _ => None,
                }
            })
            .collect();
        for (id, state, params, mut owned, handshake) in candidates {
            let Some(root) = Path::new(&owned.host_dir).parent().map(Path::to_path_buf) else {
                continue;
            };
            if owned.stop_requested {
                settle(&state, &owned, SessionState::Stopped, None);
                continue;
            }
            match find_launch(&owned) {
                Found::Exited(exit) => settle(
                    &state,
                    &owned,
                    SessionState::Failed,
                    Some(format!(
                        "Codex backend ended while the service was down ({}); resume to continue \
                         the thread",
                        match (exit.code, exit.signal) {
                            (Some(code), _) => format!("exit code {code}"),
                            (_, Some(signal)) => format!("signal {signal}"),
                            _ => "unknown status".into(),
                        }
                    )),
                ),
                Found::Gone => settle(
                    &state,
                    &owned,
                    SessionState::Failed,
                    Some(if owned.child_pid.is_none() {
                        "the service stopped before the Codex backend was launched".into()
                    } else {
                        "the Codex backend ended while the service was down".into()
                    }),
                ),
                Found::Live(host) => {
                    record_host(&state, &mut owned, &host);
                    change(&state, |data| {
                        data.state = SessionState::Starting;
                        data.last_error = None;
                    });
                    let handshake = handshake.clone().unwrap_or_else(default_handshake);
                    match Self::launch(params, Arc::clone(&state), root, owned, Some(handshake)) {
                        Ok(entry) => {
                            lock(&self.entries).insert(id, entry);
                        },
                        Err(error) => change(&state, |data| {
                            data.state = SessionState::Interrupted;
                            data.backend_pid = None;
                            data.last_error = Some(format!(
                                "could not adopt the running Codex backend: {error}; stop it \
                                 explicitly"
                            ));
                        }),
                    }
                },
                // Unconfirmed host: stays interrupted; stop or resume decides by marker.
                Found::Unconfirmed => (),
            }
        }
    }

    /// End the session's tool processes, keeping the engine and its helpers. Refused while a
    /// turn runs (cancel it first) and for launches without a tool marker.
    pub fn end_tools(&self, id: &str) -> Result<proto::ToolsEnded> {
        let owned = {
            let entries = lock(&self.entries);
            let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
            let data = lock(&entry.state);
            if !data.state.is_active() || entry.stopping.load(Ordering::Acquire) {
                return Err(rpc(proto::CONFLICT, "session is not running"));
            }
            if data.turn_id.is_some() {
                return Err(rpc(
                    proto::NOT_READY,
                    "a turn is running; cancel it first, its tools are not ended underneath it",
                ));
            }
            match data.persistence.as_ref().map(|saved| &saved.launch) {
                Some(Launch::Owned(owned)) => owned.clone(),
                _ => return Err(rpc(proto::NOT_READY, "session has no XaYa-owned launch")),
            }
        };
        Ok(proto::ToolsEnded { session_id: id.into(), ended: stop_tools(&owned)? })
    }

    pub fn list(&self) -> Vec<Session> {
        lock(&self.entries).values().map(|entry| lock(&entry.state).clone()).collect()
    }

    pub fn active_count(&self) -> u32 {
        self.list().iter().filter(|session| session.state.is_active()).count() as u32
    }

    pub fn get(&self, id: &str) -> Result<Session> {
        lock(&self.entries)
            .get(id)
            .map(|entry| lock(&entry.state).clone())
            .ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))
    }

    /// A live subscription and, with `after`, every stored event following that cursor up to
    /// the subscription's snapshot. Events are stored before delivery, so that range is already
    /// on disk; a cursor it cannot continue (other epoch, ahead, or pruned) is stale.
    pub(crate) fn subscribe(
        &self,
        id: &str,
        after: Option<&proto::EventCursor>,
    ) -> Result<(Subscription, Vec<proto::SessionEvent>)> {
        let (subscription, store) = {
            let entries = lock(&self.entries);
            let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
            let mut data = lock(&entry.state);
            (data.subscribe(Arc::clone(&self.subscribers))?, data.store())
        };
        let Some(cursor) = after else { return Ok((subscription, Vec::new())) };
        let (epoch, last) = (&subscription.snapshot.epoch, subscription.snapshot.last_seq);
        let stale = || {
            rpc(proto::STALE, "stored history does not continue this cursor; take a fresh snapshot")
        };
        if &cursor.epoch != epoch || cursor.seq > last {
            return Err(stale());
        }
        let mut replay = Vec::new();
        let mut next = cursor.seq;
        while next < last {
            let Some(store) = &store else { return Err(stale()) };
            let page = store.load_events(id, epoch, next, 1 << 20)?;
            let Some(first) = page.first() else { return Err(stale()) };
            if first.seq != next + 1 {
                return Err(stale());
            }
            for event in page {
                if event.seq != next + 1 || event.seq > last {
                    break;
                }
                next = event.seq;
                replay.push(event);
            }
        }
        Ok((subscription, replay))
    }

    /// One size-bounded page of the session's stored events after `after_seq`.
    pub fn history(&self, id: &str, after_seq: u64) -> Result<proto::EventHistory> {
        let (snapshot, store) = {
            let entries = lock(&self.entries);
            let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
            let data = lock(&entry.state);
            (data.snapshot(), data.store())
        };
        let (events, first_seq) = match &store {
            // Well below a frame: one event already fits a frame of its own.
            Some(store) => (
                store.load_events(id, &snapshot.epoch, after_seq, 32 << 10)?,
                store.first_event(id, &snapshot.epoch)?,
            ),
            None => (Vec::new(), 0),
        };
        Ok(proto::EventHistory {
            session_id: id.into(),
            epoch: snapshot.epoch,
            first_seq,
            last_seq: snapshot.last_seq,
            events: events.into_iter().filter(|event| event.seq <= snapshot.last_seq).collect(),
        })
    }

    pub fn snapshot(&self, id: &str) -> Result<proto::EventSnapshot> {
        let entries = lock(&self.entries);
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        Ok(lock(&entry.state).snapshot())
    }

    pub fn command(&self, id: &str, command_id: &str) -> Result<CommandReceipt> {
        let entries = lock(&self.entries);
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        lock(&entry.state).commands.get(command_id).map(|record| record.receipt.clone()).ok_or_else(
            || {
                rpc(
                    proto::NOT_FOUND,
                    "unknown command; an uncertain operation must not be replayed automatically",
                )
            },
        )
    }

    pub fn control(
        &self,
        id: &str,
        command_id: String,
        operation: ControlOperation,
    ) -> Result<CommandReceipt> {
        control::enqueue(self, id, command_id, operation)
    }

    pub fn attach_info(&self, id: &str) -> Result<AttachInfo> {
        let entries = lock(&self.entries);
        let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
        let session = lock(&entry.state);
        if !session.state.is_active() {
            return Err(rpc(proto::NOT_READY, "session backend is no longer running"));
        }
        if entry.stopping.load(Ordering::Acquire) {
            return Err(rpc(proto::NOT_READY, "session is stopping"));
        }
        if session.attached {
            return Err(rpc(proto::CONFLICT, "session already has an attached terminal"));
        }
        if session.engine == "claude" {
            let Some(Persistence { launch: Launch::Owned(owned), .. }) = &session.persistence
            else {
                return Err(rpc(proto::NOT_READY, "Claude is not running in an owned PTY host"));
            };
            if session.native_thread_id.is_none() {
                return Err(rpc(proto::NOT_READY, "Claude identity is not confirmed yet"));
            }
            // The terminal attaches to XaYa's own host, never to a vendor background job.
            return Ok(AttachInfo {
                session_id: id.into(),
                executable: std::env::current_exe()?
                    .to_str()
                    .ok_or(Error::Protocol("non-UTF-8 service executable"))?
                    .into(),
                codex_home: String::new(),
                cwd: session.cwd.clone(),
                args: vec!["pty-attach".into(), "--dir".into(), owned.host_dir.clone()],
                environment: BTreeMap::new(),
            });
        }
        let mut args = vec![
            "--remote".into(),
            format!("unix://{}", entry.endpoint.display()),
            "-C".into(),
            session.cwd.clone(),
        ];
        if let Some(thread) = &session.native_thread_id {
            args.extend(["resume".into(), thread.clone()]);
        } else {
            // Native remote resume rejects permission overrides (Codex 0.169.0). The thread's
            // permissions are already set by the dedicated backend and its first native start.
            args.extend([
                "--sandbox".into(),
                session.sandbox.as_str().into(),
                "--ask-for-approval".into(),
                "on-request".into(),
            ]);
        }
        Ok(AttachInfo {
            session_id: id.into(),
            executable: entry.executable.clone(),
            codex_home: entry.codex_home.clone(),
            cwd: session.cwd.clone(),
            args,
            environment: BTreeMap::new(),
        })
    }

    pub fn stop(&self, id: &str) -> Result<Session> {
        self.stop_session(id, false)
    }

    pub(crate) fn stop_idle(&self, id: &str) -> Result<Session> {
        self.stop_session(id, true)
    }

    fn stop_session(&self, id: &str, idle_only: bool) -> Result<Session> {
        // Never hold the registry lock while joining; status on other connections stays responsive.
        let (state, worker) = {
            let mut entries = lock(&self.entries);
            let entry =
                entries.get_mut(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
            {
                let data = lock(&entry.state);
                if idle_only
                    && (entry.stopping.load(Ordering::Acquire)
                        || data.state != SessionState::Ready
                        || data.attached
                        || data.turn_id.is_some()
                        || data.verification.is_some()
                        || data.pending_requests != 0
                        || data
                            .commands
                            .values()
                            .any(|c| c.receipt.status == CommandStatus::Pending))
                {
                    return Err(rpc(proto::CONFLICT, "session must be detached and idle to stop"));
                }
                entry.stopping.store(true, Ordering::Release);
            }
            (Arc::clone(&entry.state), entry.worker.take())
        };
        // Durable before any signal: a crash from here on finishes this stop on restart.
        {
            let mut data = lock(&state);
            if let Some(Persistence { launch: Launch::Owned(owned), .. }) = &mut data.persistence
                && !owned.stop_requested
            {
                owned.stop_requested = true;
                data.persist();
            }
        }
        crate::faults::crash_point("stop.after_intent");
        if let Some(worker) = worker {
            worker.join().map_err(|_| Error::Protocol("session worker panicked"))?;
        }
        // A restored or interrupted session has no worker, but XaYa still owns its processes:
        // end them by marker and release the checkout only once none remain.
        let recover = {
            let data = lock(&state);
            match &data.persistence {
                Some(Persistence { launch: Launch::Owned(owned), .. })
                    if !data.state.is_active() =>
                {
                    Some(owned.clone())
                },
                _ => None,
            }
        };
        if let Some(owned) = recover {
            if let Err(error) = stop_owned(&owned) {
                change(&state, |data| data.last_error = Some(error.to_string()));
                return Err(error);
            }
            if let Some(root) = Path::new(&owned.host_dir).parent() {
                remove_launch_root(root);
            }
            change(&state, |data| {
                data.state = SessionState::Stopped;
                data.backend_pid = None;
                data.last_error = None;
                if let Some(saved) = &mut data.persistence {
                    saved.launch = Launch::Ended;
                }
            });
            lock(&state).storage_ready()?;
        }
        let owned = lock(&state)
            .persistence
            .as_ref()
            .is_some_and(|saved| matches!(saved.launch, Launch::Owned(_)));
        let session = lock(&state).clone();
        if owned {
            return Err(rpc(
                proto::NOT_READY,
                "stop could not confirm that every owned process ended; the checkout stays owned. \
                 Inspect session show and retry stop",
            ));
        }
        if session.state.is_active() {
            return Err(rpc(proto::NOT_READY, "another client is stopping this session"));
        }
        Ok(session)
    }

    pub fn stop_all(&self) {
        let workers: Vec<_> = {
            let mut entries = lock(&self.entries);
            entries
                .values_mut()
                .filter_map(|entry| {
                    entry.stopping.store(true, Ordering::Release);
                    entry.worker.take()
                })
                .collect()
        };
        for worker in workers {
            let _ = worker.join();
        }
    }
}

const BACKEND_SOCKET: &str = "backend.sock";

fn new_owned(root: &Path) -> Result<OwnedLaunch> {
    Ok(OwnedLaunch {
        host_dir: root
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

/// What a restarted service finds of a launch recorded as owned.
pub(crate) enum Found {
    /// The host recorded the program's exit.
    Exited(pty_host::Exit),
    /// The launch's own private, token-authenticated host answers; its PIDs are authoritative
    /// even when the previous service crashed before recording them.
    Live(pty_host::Status),
    /// No host and no process carrying the launch marker: nothing of it runs.
    Gone,
    /// Neither can be confirmed; a second writer must not be started.
    Unconfirmed,
}

pub(crate) fn find_launch(owned: &OwnedLaunch) -> Found {
    let dir = Path::new(&owned.host_dir);
    match pty_host::exit_status(dir) {
        Ok(Some(exit)) => return Found::Exited(exit),
        Ok(None) => (),
        Err(_) => return Found::Unconfirmed,
    }
    if let Ok(status) = pty_host::status(dir) {
        let same = owned.child_pid.is_none_or(|pid| pid == status.child_pid)
            && owned.host_pid.is_none_or(|pid| pid == status.host_pid);
        return if same { Found::Live(status) } else { Found::Unconfirmed };
    }
    match owned.ended() {
        Ok(true) => Found::Gone,
        _ => Found::Unconfirmed,
    }
}

/// Record the PIDs a restarted service learned from the live host.
pub(crate) fn record_host(state: &SharedSession, owned: &mut OwnedLaunch, host: &pty_host::Status) {
    owned.host_pid = Some(host.host_pid);
    owned.child_pid = Some(host.child_pid);
    change(state, |data| {
        data.backend_pid = u32::try_from(host.child_pid).ok();
        if let Some(Persistence { launch: Launch::Owned(saved), .. }) = &mut data.persistence {
            saved.host_pid = owned.host_pid;
            saved.child_pid = owned.child_pid;
        }
    });
    lock(state).persist();
}

/// Settle a launch whose processes are all gone (or finish a stop that was asked for).
pub(crate) fn settle(
    state: &SharedSession,
    owned: &OwnedLaunch,
    outcome: SessionState,
    error: Option<String>,
) {
    let ended = stop_owned(owned);
    if ended.is_ok()
        && let Some(root) = Path::new(&owned.host_dir).parent()
    {
        remove_launch_root(root);
    }
    change(state, |data| {
        data.backend_pid = None;
        data.turn_id = None;
        match &ended {
            Ok(()) => {
                data.state = outcome;
                data.last_error = error;
                if let Some(saved) = &mut data.persistence {
                    saved.launch = Launch::Ended;
                }
            },
            Err(failure) => {
                data.state = SessionState::Interrupted;
                data.last_error = Some(format!("owned processes could not be ended: {failure}"));
            },
        }
    });
    lock(state).persist();
}

/// Removes a launch directory once its host is confirmed gone. Only `<root>/h` layouts that
/// this module created are touched.
fn remove_launch_root(root: &Path) {
    let owned = root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("codex-"));
    if owned && fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir()) {
        let _ = fs::remove_dir_all(root);
    }
}

/// Used when no native client ever initialized this backend before the service restarted.
fn default_handshake() -> Value {
    json!({"clientInfo":{"name":"xaya","title":"XaYa","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}})
}

async fn spawn_host(params: &StartSessionParams, owned: &OwnedLaunch) -> Result<pty_host::Host> {
    let socket = Path::new(&owned.host_dir).join(BACKEND_SOCKET);
    let mut argv = vec![
        params.executable.clone(),
        "app-server".into(),
        "--listen".into(),
        format!("unix://{}", socket.to_str().ok_or(Error::Protocol("non-UTF-8 socket"))?),
        "-c".into(),
        format!("sandbox_mode=\"{}\"", params.sandbox.as_str()),
        "-c".into(),
        "approval_policy=\"on-request\"".into(),
    ];
    // Exec tools (and only they) carry the tool marker, so they can be ended on their own.
    if let Some(tool) = &owned.tool_marker {
        argv.extend([
            "-c".into(),
            format!("shell_environment_policy.set.{}=\"{tool}\"", crate::ownership::TOOL_ENV),
        ]);
    }
    let env = vec![
        ("CODEX_HOME".to_owned(), params.codex_home.clone()),
        (crate::ownership::OWNER_ENV.to_owned(), owned.marker.clone()),
    ];
    let (executable, dir, cwd) =
        (std::env::current_exe()?, PathBuf::from(&owned.host_dir), PathBuf::from(&params.cwd));
    tokio::task::spawn_blocking(move || {
        pty_host::spawn(&executable, &dir, &cwd, &argv, &env, pty_host::Size { rows: 24, cols: 80 })
    })
    .await
    .map_err(|_| Error::Protocol("PTY host launch panicked"))?
}

/// The backend socket, or the private short socket Codex links to when the path is too long.
fn private_socket(path: &Path) -> io::Result<PathBuf> {
    let refused =
        || io::Error::new(io::ErrorKind::PermissionDenied, "backend socket is not private");
    let target = if fs::symlink_metadata(path)?.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let parent = target.parent().filter(|_| target.is_absolute()).ok_or_else(refused)?;
        let meta = fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != crate::runtime::user_id() || meta.mode() & 0o077 != 0 {
            return Err(refused());
        }
        target
    } else {
        path.to_owned()
    };
    let meta = fs::symlink_metadata(&target)?;
    if !meta.file_type().is_socket() || meta.uid() != crate::runtime::user_id() {
        return Err(refused());
    }
    Ok(target)
}

async fn connect_backend(owned: &OwnedLaunch, stopping: &AtomicBool) -> Result<BackendSocket> {
    let dir = Path::new(&owned.host_dir);
    let socket = dir.join(BACKEND_SOCKET);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let stream = loop {
        if stopping.load(Ordering::Acquire) {
            return Err(Error::Protocol("session stopped before the Codex backend was ready"));
        }
        if pty_host::exit_status(dir)?.is_some() {
            return Err(Error::Protocol("Codex backend exited before accepting connections"));
        }
        match private_socket(&socket) {
            Ok(target) => {
                if let Ok(stream) = UnixStream::connect(&target).await {
                    break stream;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Protocol("Codex backend did not open its socket"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(ENGINE_FRAME_BYTES))
        .max_frame_size(Some(ENGINE_FRAME_BYTES));
    let (socket, _) = timeout(
        Duration::from_secs(5),
        client_async_with_config("ws://localhost/", stream, Some(config)),
    )
    .await
    .map_err(|_| Error::Protocol("Codex backend handshake timed out"))?
    .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(socket)
}

/// End every process of an owned launch: hang up the program through its host, escalate, then
/// kill identity-checked marked survivors (e.g. setsid'd background tools). Errors leave the
/// launch owned.
/// End every tool process of a launch (SIGTERM, then SIGKILL) and confirm none is left.
/// Only processes carrying both the owner and the tool marker are touched. Returns how many
/// distinct processes were ended.
pub(crate) fn stop_tools(owned: &OwnedLaunch) -> Result<u32> {
    let tool = owned.tool_marker.as_deref().ok_or_else(|| {
        rpc(
            proto::NOT_READY,
            "this launch has no tool marker (started by an older service); stop the session to \
             end its tools",
        )
    })?;
    let markers =
        [(crate::ownership::OWNER_ENV, owned.marker.as_str()), (crate::ownership::TOOL_ENV, tool)];
    let registered = registered_tool_shells(owned);
    let scan = || crate::ownership::scan_tools(&owned.marker, tool, &registered);
    let mut ended = std::collections::BTreeSet::new();
    for signal in [libc::SIGTERM, libc::SIGKILL, libc::SIGKILL, libc::SIGKILL] {
        let survivors = scan()?;
        if survivors.is_empty() {
            return Ok(ended.len() as u32);
        }
        ended.extend(survivors.iter().map(|process| (process.pid, process.started)));
        crate::ownership::signal_all(&survivors, &markers, signal)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !scan()?.is_empty() {
            thread::sleep(Duration::from_millis(50));
        }
    }
    if scan()?.is_empty() {
        Ok(ended.len() as u32)
    } else {
        Err(Error::Protocol("tool processes survived termination"))
    }
}

/// Where a launch's tool shells register themselves: next to its host directory.
pub(crate) const TOOL_SHELLS: &str = "tool-shells";
/// Only the newest registrations are read; older shells have long finished.
const TOOL_SHELLS_TAIL: u64 = 1 << 20;

/// `(pid, parent pid)` lines the launch's tool shells appended. Unreadable or malformed content
/// only means fewer tools are found; it never widens what is signalled.
fn registered_tool_shells(owned: &OwnedLaunch) -> Vec<(i32, i32)> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = Path::new(&owned.host_dir).parent().map(|dir| dir.join(TOOL_SHELLS)) else {
        return Vec::new();
    };
    let mut text = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(TOOL_SHELLS_TAIL)))?;
        file.take(TOOL_SHELLS_TAIL).read_to_end(&mut text)
    });
    if read.is_err() {
        return Vec::new();
    }
    parse_tool_shells(&String::from_utf8_lossy(&text))
}

fn parse_tool_shells(text: &str) -> Vec<(i32, i32)> {
    let mut shells: Vec<(i32, i32)> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            (fields.next().is_none() && pid > 1 && ppid > 1).then_some((pid, ppid))
        })
        .collect();
    shells.sort_unstable();
    shells.dedup();
    shells
}

pub(crate) fn stop_owned(owned: &OwnedLaunch) -> Result<()> {
    let dir = Path::new(&owned.host_dir);
    let markers = [(crate::ownership::OWNER_ENV, owned.marker.as_str())];
    // Observe the launch while its program still holds it together: a process whose
    // environment the platform hides is attributed through its owned parent or session, and
    // stays attributed by identity once the hang-up orphans it.
    let mut seen = crate::ownership::scan(&owned.marker)?;
    let gone = |wait: Duration| {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if pty_host::exit_status(dir).ok().flatten().is_some() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    };
    if pty_host::exit_status(dir)?.is_none() {
        for (signal, wait) in [(libc::SIGHUP, 10), (libc::SIGTERM, 5), (libc::SIGKILL, 5)] {
            if pty_host::signal(dir, signal).is_err() || gone(Duration::from_secs(wait)) {
                break;
            }
        }
    }
    for _ in 0..3 {
        let survivors = crate::ownership::scan_with(&markers, &seen)?;
        if survivors.is_empty() {
            break;
        }
        seen.extend(survivors.iter().cloned());
        crate::ownership::signal_all(&survivors, &markers, libc::SIGKILL)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && !crate::ownership::scan_with(&markers, &seen)?.is_empty()
        {
            thread::sleep(Duration::from_millis(50));
        }
    }
    if owned.ended()? {
        Ok(())
    } else {
        Err(Error::Protocol("owned processes survived termination; checkout stays owned"))
    }
}

fn change(state: &SharedSession, update: impl FnOnce(&mut SessionData)) {
    let mut state = lock(state);
    let before = state.session.clone();
    update(&mut state);
    if before != state.session {
        state.changed();
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct Pending {
    original_id: Value,
    method: String,
    generation: u64,
    command_id: Option<String>,
}

struct ServerRequest {
    thread_id: Option<String>,
    turn_id: Option<String>,
    approval_id: Option<String>,
    native_only: bool,
}

#[derive(Default)]
struct Protocol {
    approval_epoch: String,
    file_items: BTreeMap<String, (String, Value)>,
    sequence: u64,
    generation: u64,
    initialized: Option<Value>,
    initialized_sent: bool,
    pending: HashMap<String, Pending>,
    // Retain only routing metadata. Codex replays outstanding requests on native thread/resume;
    // XaYa tracks their lifetime to discard duplicate/expired native answers.
    requests: BTreeMap<String, ServerRequest>,
}

impl Protocol {
    fn tui_request(
        &mut self,
        message: &mut Value,
        session: &mut SessionData,
    ) -> Result<Option<Value>> {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if session.verification.is_some() && method != "initialized" {
            return Ok(Some(match message.get("id") {
                Some(id) if !method.is_empty() => {
                    json!({"id":id,"error":{"code":proto::CONFLICT,"message":"workspace checkpoint in progress"}})
                },
                _ => Value::Null,
            }));
        }
        if method == "initialized" {
            if self.initialized_sent {
                return Ok(Some(Value::Null));
            }
            self.initialized_sent = true;
        }
        if let Some(id) = message.get("id").cloned() {
            if !method.is_empty() {
                // The native TUI can unsubscribe when it exits. Keep the host's subscription
                // alive so a detached managed session still receives events and control turns.
                if method == "thread/unsubscribe"
                    && session.native_thread_id.as_deref().is_some_and(|thread| {
                        message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread)
                    })
                {
                    return Ok(Some(json!({"id":id,"result":{"status":"unsubscribed"}})));
                }
                if method == "turn/start"
                    && (session.turn_id.is_some()
                        || self.pending.values().any(|request| request.method == "turn/start"))
                {
                    return Ok(Some(
                        json!({"id":id,"error":{"code":proto::CONFLICT,"message":"a turn or send is already active"}}),
                    ));
                }
                if method == "thread/fork"
                    || (matches!(method, "thread/start" | "thread/resume")
                        && self.pending.values().any(|request| {
                            matches!(request.method.as_str(), "thread/start" | "thread/resume")
                        }))
                {
                    return Ok(Some(json!({"id":id,"error":{"code":proto::CONFLICT,
                        "message":"a managed session owns one main thread; another binding is not available"}})));
                }
                if method == "initialize" {
                    if let Some(result) = &self.initialized {
                        return Ok(Some(json!({"id":id,"result":result})));
                    }
                    // Kept so a restarted service can join this backend as the same client.
                    if let Some(persistence) = &mut session.persistence {
                        persistence.handshake =
                            Some(message.get("params").cloned().unwrap_or_else(|| json!({})));
                    }
                    session.persist();
                }
                if let Some(thread) = &session.native_thread_id
                    && (method == "thread/start"
                        || (method == "thread/resume"
                            && message.pointer("/params/threadId").and_then(Value::as_str)
                                != Some(thread)))
                {
                    return Ok(Some(json!({"id":id,"error":{"code":proto::CONFLICT,
                            "message":"managed session is bound to another thread; start a new XaYa session"}})));
                }
                if self.pending.len() >= MAX_PENDING {
                    return Err(Error::Protocol("too many outstanding native requests"));
                }
                self.sequence += 1;
                let proxy_id = format!("xaya-tui:{}", self.sequence);
                self.pending.insert(proxy_id.clone(), Pending {
                    original_id: id,
                    method: method.into(),
                    generation: self.generation,
                    command_id: None,
                });
                session.pending_requests = self.pending.len();
                message["id"] = json!(proxy_id);
            } else {
                match self.requests.remove(&id.to_string()) {
                    Some(request) => self.finish_request(request, session),
                    None => return Ok(Some(Value::Null)),
                }
            }
        }
        Ok(None)
    }

    fn backend_message(&mut self, message: &mut Value, session: &mut SessionData) -> Result<bool> {
        let before = session.session.clone();
        let result = self.reduce_backend(message, session);
        if before != session.session {
            session.changed();
        }
        result
    }

    fn reduce_backend(&mut self, message: &mut Value, session: &mut SessionData) -> Result<bool> {
        if message.get("method").is_none() {
            let Some(id) = message.get("id").and_then(Value::as_str) else { return Ok(false) };
            let Some(pending) = self.pending.remove(id) else { return Ok(false) };
            session.pending_requests = self.pending.len();
            if let Some(command_id) = pending.command_id {
                let error = message.pointer("/error/message").and_then(Value::as_str);
                session.command_changed(CommandReceipt {
                    command_id,
                    status: if message.get("error").is_some() {
                        CommandStatus::Failed
                    } else {
                        CommandStatus::Succeeded
                    },
                    turn_id: message
                        .pointer("/result/turn/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    error: error.map(|text| text.chars().take(256).collect()),
                });
                return Ok(false);
            }
            if let Some(result) = message.get("result") {
                if pending.method == "initialize" {
                    self.initialized = Some(result.clone());
                    session.state = SessionState::Ready;
                }
                if matches!(pending.method.as_str(), "thread/start" | "thread/resume") {
                    let thread = result
                        .pointer("/thread/id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty() && id.len() <= 256)
                        .ok_or(Error::Protocol("thread response missing a valid native ID"))?;
                    if session.native_thread_id.as_deref().is_some_and(|bound| bound != thread) {
                        return Err(Error::Protocol("Codex returned a different native thread"));
                    }
                    session.native_thread_id = Some(thread.into());
                }
            }
            message["id"] = pending.original_id;
            return Ok(pending.generation == self.generation);
        }
        let method = message["method"].as_str().unwrap_or("");
        if method == "item/started"
            && message.pointer("/params/item/type").and_then(Value::as_str) == Some("fileChange")
            && session.native_thread_id.as_deref().is_some_and(|thread| {
                message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread)
            })
            && let Some(id) = message
                .pointer("/params/item/id")
                .and_then(Value::as_str)
                .filter(|id| id.len() <= 256)
            && (self.file_items.len() < MAX_PENDING || self.file_items.contains_key(id))
            && let Some(turn) = message
                .pointer("/params/turnId")
                .and_then(Value::as_str)
                .filter(|turn| turn.len() <= 256)
        {
            let changes = message.pointer("/params/item/changes").cloned().unwrap_or(Value::Null);
            self.file_items.insert(
                id.into(),
                (
                    turn.into(),
                    if serde_json::to_vec(&changes)?.len() <= 2048 { changes } else { Value::Null },
                ),
            );
        }
        if let Some(id) = message.get("id")
            && !self.requests.contains_key(&id.to_string())
        {
            if self.requests.len() >= MAX_PENDING {
                return Err(Error::Protocol("too many unanswered Codex requests"));
            }
            let known_approval = matches!(
                method,
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
            ) && session.native_thread_id.as_deref().is_some_and(|thread| {
                message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread)
            });
            let mut approval_id = None;
            let mut native_only = false;
            if known_approval {
                if session.approvals.len() < 16 {
                    self.sequence += 1;
                    let id = format!("approval-{}-{}", self.approval_epoch, self.sequence);
                    let mut details = message.get("params").cloned().unwrap_or(Value::Null);
                    let file_change = method == "item/fileChange/requestApproval";
                    let changes = message
                        .pointer("/params/itemId")
                        .and_then(Value::as_str)
                        .and_then(|id| self.file_items.get(id))
                        .filter(|(turn, _)| {
                            Some(turn.as_str())
                                == message.pointer("/params/turnId").and_then(Value::as_str)
                        })
                        .map(|(_, changes)| changes);
                    let has_context =
                        !file_change || changes.is_some_and(|changes| !changes.is_null());
                    if file_change && let Some(changes) = changes {
                        details["changes"] = changes.clone();
                    }
                    let controllable = has_context && serde_json::to_vec(&details)?.len() <= 2048;
                    let decisions = [
                        proto::ApprovalDecision::Accept,
                        proto::ApprovalDecision::Decline,
                        proto::ApprovalDecision::Cancel,
                    ]
                    .into_iter()
                    .filter(|decision| {
                        details.get("availableDecisions").and_then(Value::as_array).is_none_or(
                            |choices| {
                                choices.iter().any(|choice| {
                                    serde_json::to_value(decision)
                                        .is_ok_and(|value| value == *choice)
                                })
                            },
                        )
                    })
                    .collect();
                    if let Some(fields) = details.as_object_mut() {
                        fields.retain(|key, _| {
                            !["threadId", "turnId", "itemId", "startedAtMs"].contains(&key.as_str())
                        });
                    }
                    let approval = proto::Approval {
                        id: id.clone(),
                        kind: if method == "item/fileChange/requestApproval" {
                            proto::ApprovalKind::FileChange
                        } else {
                            proto::ApprovalKind::Command
                        },
                        decisions,
                        method: method.into(),
                        turn_id: message
                            .pointer("/params/turnId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        details: if controllable {
                            details
                        } else {
                            json!({"message":"Complete review details are unavailable or exceed the control limit; answer in the native TUI"})
                        },
                        controllable,
                    };
                    session.approvals.insert(id.clone(), approval.clone());
                    session.publish(EventPayload::ApprovalRequested(approval));
                    approval_id = Some(id);
                } else {
                    native_only = true;
                    session.native_only_approvals += 1;
                    session.publish(EventPayload::NativeOnlyApprovals {
                        count: session.native_only_approvals,
                    });
                }
            }
            self.requests.insert(id.to_string(), ServerRequest {
                thread_id: message
                    .pointer("/params/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                approval_id,
                native_only,
                turn_id: message
                    .pointer("/params/turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        if method == "serverRequest/resolved"
            && let Some(id) = message.pointer("/params/requestId")
        {
            if let Some(request) = self.requests.remove(&id.to_string()) {
                self.finish_request(request, session);
            }
            if session.state == SessionState::Blocked
                && session.turn_id.is_some()
                && !self.requests.values().any(|request| {
                    request.thread_id.as_deref() == session.native_thread_id.as_deref()
                })
            {
                session.state = SessionState::Working;
            }
        }
        let main = session.native_thread_id.as_deref().is_some_and(|id| {
            message.pointer("/params/threadId").and_then(Value::as_str) == Some(id)
        });
        if main {
            match method {
                "turn/started" => {
                    session.turn_id = message
                        .pointer("/params/turn/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    session.state = SessionState::Working;
                },
                "item/agentMessage/delta" => {
                    let turn_id =
                        message.pointer("/params/turnId").and_then(Value::as_str).unwrap_or("");
                    let item_id =
                        message.pointer("/params/itemId").and_then(Value::as_str).unwrap_or("");
                    if turn_id.len() <= 256 && item_id.len() <= 256 {
                        let text =
                            message.pointer("/params/delta").and_then(Value::as_str).unwrap_or("");
                        let mut chunk = String::new();
                        for ch in text.chars() {
                            chunk.push(ch);
                            if chunk.len() >= 1024 {
                                session.publish(EventPayload::TextDelta {
                                    turn_id: turn_id.into(),
                                    item_id: item_id.into(),
                                    text: std::mem::take(&mut chunk),
                                });
                            }
                        }
                        if !chunk.is_empty() {
                            session.publish(EventPayload::TextDelta {
                                turn_id: turn_id.into(),
                                item_id: item_id.into(),
                                text: chunk,
                            });
                        }
                    }
                },
                "turn/completed" => {
                    if message.pointer("/params/turn/id").and_then(Value::as_str)
                        == session.turn_id.as_deref()
                    {
                        let completed_turn = session.turn_id.take();
                        self.file_items.clear();
                        session.state = SessionState::Ready;
                        session.last_error = message
                            .pointer("/params/turn/error/message")
                            .and_then(Value::as_str)
                            .map(|text| text.chars().take(512).collect());
                        let expired: Vec<_> = self
                            .requests
                            .iter()
                            .filter(|(_, request)| {
                                request.thread_id.as_deref() == session.native_thread_id.as_deref()
                                    && (request.turn_id.is_none()
                                        || request.turn_id == completed_turn)
                            })
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in expired {
                            if let Some(request) = self.requests.remove(&id) {
                                self.finish_request(request, session);
                            }
                        }
                        session.publish(EventPayload::TurnCompleted {
                            turn_id: completed_turn.unwrap_or_default(),
                            status: message
                                .pointer("/params/turn/status")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .chars()
                                .take(64)
                                .collect(),
                        });
                    }
                },
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    session.state = SessionState::Blocked;
                },
                _ => (), // Forward unknown vendor events unchanged.
            }
        }
        Ok(true)
    }
}

async fn next_terminal(
    terminal: &mut Option<Terminal>,
) -> Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match terminal {
        Some(terminal) => terminal.next().await,
        None => std::future::pending().await,
    }
}

async fn send_terminal(terminal: &mut Option<Terminal>, message: Message, state: &SharedSession) {
    if let Some(socket) = terminal
        && matches!(timeout(WRITE_TIMEOUT, socket.send(message)).await, Ok(Ok(())))
    {
        return;
    }
    *terminal = None;
    change(state, |state| state.attached = false);
}

async fn send_backend(backend: &mut BackendSocket, message: &Value) -> Result<()> {
    timeout(WRITE_TIMEOUT, backend.send(Message::text(message.to_string())))
        .await
        .map_err(|_| Error::Protocol("Codex write timed out; query command before retrying"))?
        .map_err(|error| io::Error::other(error.to_string()).into())
}

/// Next JSON frame from the backend; pings are answered and other control frames skipped.
async fn next_backend(backend: &mut BackendSocket) -> Result<Value> {
    loop {
        match backend.next().await {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Ping(data))) => {
                timeout(WRITE_TIMEOUT, backend.send(Message::Pong(data)))
                    .await
                    .map_err(|_| Error::Protocol("Codex pong timed out"))?
                    .map_err(|error| io::Error::other(error.to_string()))?;
            },
            Some(Ok(Message::Pong(_) | Message::Frame(_))) => (),
            Some(Ok(Message::Binary(_))) => {
                return Err(Error::Protocol("Codex backend sent a binary frame"));
            },
            Some(Err(error)) => return Err(io::Error::other(error.to_string()).into()),
            Some(Ok(Message::Close(_))) | None => {
                return Err(Error::Protocol("Codex backend closed its connection"));
            },
        }
    }
}

/// An adoption-internal request. Everything else the backend sends meanwhile, such as an
/// approval it re-sends for the resumed thread, goes through the normal reducer.
async fn adopt_call(
    backend: &mut BackendSocket,
    protocol: &mut Protocol,
    state: &SharedSession,
    method: &str,
    params: Value,
) -> Result<Value> {
    let id = format!("xaya-adopt:{method}");
    send_backend(backend, &json!({"id":id,"method":method,"params":params})).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut message = tokio::time::timeout_at(deadline, next_backend(backend))
            .await
            .map_err(|_| Error::Protocol("Codex backend did not answer during adoption"))??;
        if message.get("method").is_none() && message.get("id").and_then(Value::as_str) == Some(&id)
        {
            if let Some(error) = message.get("error") {
                return Err(Error::Rpc {
                    code: proto::ENGINE_ERROR,
                    message: format!("Codex {method} failed during adoption: {error}")
                        .chars()
                        .take(512)
                        .collect(),
                });
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
        protocol.backend_message(&mut message, &mut lock(state))?;
    }
}

/// The command ID under which a headless session's first message is sent, once.
pub(crate) const FIRST_MESSAGE: &str = "xaya-first-message";

/// Bind a main thread and send the first message of a session that has no native TUI. Durable
/// at each point: a bound thread is reused, and the message is sent only while no command
/// record exists for it, so a restart never sends it twice.
async fn start_headless(
    backend: &mut BackendSocket,
    protocol: &mut Protocol,
    state: &SharedSession,
    cwd: &str,
    prompt: &str,
) -> Result<()> {
    if lock(state).native_thread_id.is_none() {
        let started =
            adopt_call(backend, protocol, state, "thread/start", json!({"cwd":cwd})).await?;
        let thread = started
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or(Error::Protocol("Codex started no valid main thread"))?
            .to_owned();
        change(state, |session| session.native_thread_id = Some(thread));
        lock(state).persist();
        lock(state).storage_ready()?;
        crate::faults::crash_point("headless.after_thread");
    }
    // The record is durable before the message is written to Codex: without one, the message
    // was never sent; with one, whatever became of it, it is never sent again. (Codex 0.170
    // cannot list the turns of a thread that has none, so it cannot be asked instead.)
    if lock(state).commands.contains_key(FIRST_MESSAGE) {
        return Ok(());
    }
    let command = ControlRequest {
        command_id: FIRST_MESSAGE.into(),
        operation: proto::ControlOperation::Send { text: prompt.into() },
        queued_at: std::time::Instant::now(),
    };
    {
        let mut session = lock(state);
        let receipt = CommandReceipt {
            command_id: FIRST_MESSAGE.into(),
            status: CommandStatus::Pending,
            turn_id: None,
            error: None,
        };
        session.commands.insert(FIRST_MESSAGE.into(), crate::events::CommandRecord {
            operation: command.operation.clone(),
            receipt: receipt.clone(),
        });
        session.publish(EventPayload::CommandChanged(receipt));
        session.storage_ready()?;
    }
    let (message, _) = protocol.prepare_control(&command, &mut lock(state))?;
    if send_backend(backend, &message).await.is_err() {
        return Err(Error::Protocol("Codex first message write failed; query it before retrying"));
    }
    crate::faults::crash_point("headless.after_send");
    Ok(())
}

/// Rebuilds session state on a new connection to a backend that outlived the previous service.
/// The backend, not the stored record, is the authority on whether a turn is still running.
async fn adopt(
    backend: &mut BackendSocket,
    protocol: &mut Protocol,
    state: &SharedSession,
    handshake: Value,
) -> Result<()> {
    let initialized = adopt_call(backend, protocol, state, "initialize", handshake).await?;
    protocol.initialized = Some(initialized);
    protocol.initialized_sent = true;
    send_backend(backend, &json!({"method":"initialized","params":{}})).await?;
    let thread = lock(state).native_thread_id.clone();
    let mut running = None;
    if let Some(thread) = thread {
        let resumed =
            adopt_call(backend, protocol, state, "thread/resume", json!({"threadId":thread}))
                .await?;
        if resumed.pointer("/thread/id").and_then(Value::as_str) != Some(thread.as_str()) {
            return Err(Error::Protocol("Codex resumed a different native thread"));
        }
        let read = adopt_call(
            backend,
            protocol,
            state,
            "thread/read",
            json!({"threadId":thread,"includeTurns":true}),
        )
        .await?;
        running = read
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .and_then(|turns| turns.last())
            .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
            .and_then(|turn| turn.get("id").and_then(Value::as_str))
            .filter(|id| id.len() <= 256)
            .map(str::to_owned);
    }
    let blocked = {
        let session = lock(state);
        protocol.requests.values().any(|request| {
            request.thread_id.is_some() && request.thread_id == session.native_thread_id
        })
    };
    change(state, |session| {
        session.state = match (&running, blocked) {
            (Some(_), true) => SessionState::Blocked,
            (Some(_), false) => SessionState::Working,
            (None, _) => SessionState::Ready,
        };
        session.turn_id = running;
    });
    Ok(())
}

async fn run(
    mut backend: BackendSocket,
    listener: UnixListener,
    state: &SharedSession,
    stopping: &AtomicBool,
    mut commands: tokio::sync::mpsc::Receiver<ControlRequest>,
    adoption: Option<Value>,
    headless: Option<(String, String)>,
) -> Result<()> {
    let mut terminal = None;
    let mut handshakes = FuturesUnordered::new();
    let mut protocol = Protocol { approval_epoch: crate::storage::epoch()?, ..Default::default() };
    if let Some(handshake) = adoption {
        adopt(&mut backend, &mut protocol, state, handshake).await?;
    }
    if let Some((cwd, prompt)) = &headless {
        start_headless(&mut backend, &mut protocol, state, cwd, prompt).await?;
    }
    let mut tick = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            Some(command) = commands.recv() => {
                let prepared = protocol.prepare_control(&command, &mut lock(state));
                match prepared {
                    Err(error) => control::fail(&mut lock(state), &command.command_id, &error.to_string()),
                    Ok((message, immediate)) => {
                        lock(state).storage_ready()?;
                        if send_backend(&mut backend, &message).await.is_err() {
                            return Err(Error::Protocol("Codex control write failed; query command before retrying"));
                        }
                        if immediate {
                            lock(state).command_changed(CommandReceipt { command_id: command.command_id,
                                status: CommandStatus::Succeeded, turn_id: None, error: None });
                        }
                    },
                }
            },
            _ = tick.tick() => {
                lock(state).storage_ready()?;
                if stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
            },
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if terminal.is_none() && handshakes.len() < 4 {
                    let config = WebSocketConfig::default().max_message_size(Some(ENGINE_FRAME_BYTES))
                        .max_frame_size(Some(ENGINE_FRAME_BYTES));
                    handshakes.push(timeout(WRITE_TIMEOUT,
                        accept_async_with_config(stream, Some(config))));
                }
            },
            connected = handshakes.next(), if !handshakes.is_empty() => {
                if let Some(Ok(Ok(socket))) = connected && terminal.is_none() {
                        terminal = Some(socket);
                        protocol.generation += 1;
                        change(state, |state| state.attached = true);
                }
            },
            message = next_terminal(&mut terminal) => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let mut message: Value = match serde_json::from_str::<Value>(&text) {
                            Ok(message) if message.is_object() => message,
                            _ => { terminal = None; change(state, |state| state.attached = false); continue; },
                        };
                        let reply = protocol.tui_request(&mut message, &mut lock(state))?;
                        match reply {
                            Some(reply) if !reply.is_null() => {
                                send_terminal(&mut terminal, Message::text(reply.to_string()), state).await;
                            },
                            Some(_) => (),
                            None => {
                                lock(state).storage_ready()?;
                                send_backend(&mut backend, &message).await?;
                            },
                        }
                    },
                    Some(Ok(Message::Ping(data))) => {
                        send_terminal(&mut terminal, Message::Pong(data), state).await;
                    },
                    Some(Ok(Message::Pong(_))) => (),
                    _ => { terminal = None; change(state, |state| state.attached = false); },
                }
            },
            message = next_backend(&mut backend) => {
                let mut message = message?;
                let forward = protocol.backend_message(&mut message, &mut lock(state))?;
                if forward {
                    send_terminal(&mut terminal, Message::text(message.to_string()), state).await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_shell_registrations_are_parsed_strictly() {
        let text = "21 20\n21 20\n7 3\nnot a pid\n1 20\n22 1\n23 20 extra\n-5 20\n24";
        assert_eq!(parse_tool_shells(text), vec![(7, 3), (21, 20)]);
        assert!(parse_tool_shells("").is_empty());
    }

    fn session() -> SessionData {
        SessionData::new(Session {
            id: "xaya-test".into(),
            engine: "codex".into(),
            cwd: "/tmp".into(),
            sandbox: proto::Sandbox::ReadOnly,
            state: SessionState::Starting,
            native_thread_id: None,
            native_job_id: None,
            turn_id: None,
            attached: true,
            backend_pid: Some(123),
            last_error: None,
            verification: None,
        })
    }

    #[test]
    fn unresolved_landing_gates_its_target_until_settled_without_its_origin_session() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (target, other) = (base.join("target"), base.join("other"));
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&other).unwrap();
        let runtime = crate::runtime::Runtime::prepare(Some(base.join("run"))).unwrap();
        let store = crate::storage::Store::open(&runtime, None).unwrap();
        let mut preview: proto::LandPreview = serde_json::from_value(serde_json::json!({
            "id":"pending-landing", "checkpoint_id":"checkpoint", "check_id":"check",
            "command_id":"create", "session_id":"evicted-session", "source_root":"/source",
            "checkpoint_commit":"commit", "checkpoint_tree":"tree", "checkpoint_base":"base",
            "checkpoint_ref":"refs/xaya/checkpoints/old", "target_ref":"refs/heads/main",
            "target_worktree":target, "status":"interrupted", "landing_outcome":"unknown",
            "stale":true, "integration_verified":true, "output_truncated":false,
            "created_at_ms":0
        }))
        .unwrap();
        store.save_preview(&preview).unwrap();
        let sessions = Sessions::open(Arc::clone(&store)).unwrap();
        // New writers and checkpoints are refused in and above the target, not elsewhere.
        assert!(sessions.workspace_available(&target).is_err());
        assert!(sessions.workspace_available(&base).is_err());
        assert!(sessions.workspace_available(&other).is_ok());
        assert!(sessions.freeze_workspace("any", &target, "cp").is_err());
        assert!(
            sessions
                .freeze_workspace_for_landing(None, &target, "lease", "another-landing")
                .is_err()
        );
        // Only this landing's own recovery passes, and it does not need the evicted session.
        let lease = sessions
            .freeze_workspace_for_landing(None, &target, "lease", "pending-landing")
            .unwrap();
        assert!(sessions.workspace_available(&target).is_err());
        drop(lease);
        preview.status = proto::LandStatus::NeedsAttention;
        preview.landing_outcome = Some(proto::LandOutcome::NeedsAttention);
        store.save_preview(&preview).unwrap();
        assert!(sessions.workspace_available(&target).is_ok());
    }

    #[test]
    fn workspace_gate_blocks_both_inputs_pending_requests_and_new_writers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let sessions = Sessions::default();
        let mut data = session();
        data.cwd = root.to_str().unwrap().into();
        data.state = SessionState::Ready;
        data.native_thread_id = Some("main".into());
        let state = Arc::new(Mutex::new(data));
        let (controller, _receiver) = tokio::sync::mpsc::channel(1);
        lock(&sessions.entries).insert("xaya-test".into(), ManagedSession {
            state: Arc::clone(&state),
            executable: String::new(),
            codex_home: String::new(),
            endpoint: PathBuf::new(),
            stopping: Arc::default(),
            worker: None,
            controller,
        });
        let mut protocol = Protocol::default();
        let mut native = json!({"id":1,"method":"turn/start"});
        protocol.tui_request(&mut native, &mut lock(&state)).unwrap();
        assert!(sessions.freeze_workspace("xaya-test", &root, "cp").is_err());
        protocol
            .backend_message(
                &mut json!({"id":native["id"],"error":{"message":"fixture"}}),
                &mut lock(&state),
            )
            .unwrap();
        let lease = sessions.freeze_workspace("xaya-test", &root, "cp").unwrap();
        assert!(sessions.workspace_available(&root).is_err());
        let native = protocol
            .tui_request(&mut json!({"id":2,"method":"turn/start"}), &mut lock(&state))
            .unwrap()
            .unwrap();
        assert_eq!(native["error"]["code"], proto::CONFLICT);
        assert!(
            sessions
                .control("xaya-test", "send".into(), ControlOperation::Send {
                    text: "blocked".into()
                })
                .is_err()
        );
        assert_eq!(sessions.get("xaya-test").unwrap().verification.as_deref(), Some("cp"));
        drop(lease);
        assert!(sessions.workspace_available(&root).is_ok());
        assert!(sessions.get("xaya-test").unwrap().verification.is_none());
        lock(&state).state = SessionState::Working;
        assert!(sessions.freeze_workspace("xaya-test", &root, "next").is_err());
    }

    #[test]
    fn thread_binding_uses_response_not_auxiliary_broadcast() {
        let mut protocol = Protocol::default();
        let mut session = session();
        protocol
            .backend_message(
                &mut json!({"method":"thread/started", "params":{"thread":{"id":"auxiliary"}}}),
                &mut session,
            )
            .unwrap();
        assert!(session.native_thread_id.is_none());
        let mut request = json!({"id":7,"method":"thread/start"});
        assert!(protocol.tui_request(&mut request, &mut session).unwrap().is_none());
        let mut reply = json!({"id":request["id"], "result":{"thread":{"id":"main"}}});
        assert!(protocol.backend_message(&mut reply, &mut session).unwrap());
        assert_eq!(reply["id"], 7);
        assert_eq!(session.native_thread_id.as_deref(), Some("main"));
    }

    #[test]
    fn reused_terminal_ids_do_not_receive_a_previous_attachments_response() {
        let mut protocol = Protocol::default();
        let mut session = session();
        let mut old = json!({"id":1,"method":"config/read"});
        protocol.tui_request(&mut old, &mut session).unwrap();
        protocol.generation += 1;
        let mut new = json!({"id":1,"method":"config/read"});
        protocol.tui_request(&mut new, &mut session).unwrap();
        assert_ne!(old["id"], new["id"]);
        assert!(
            !protocol
                .backend_message(&mut json!({"id":old["id"],"result":{}}), &mut session)
                .unwrap()
        );
        let mut reply = json!({"id":new["id"],"result":{}});
        assert!(protocol.backend_message(&mut reply, &mut session).unwrap());
        assert_eq!(reply["id"], 1);
    }

    #[test]
    fn initialization_is_forwarded_once_and_cached_for_reattach() {
        let mut protocol = Protocol::default();
        let mut session = session();
        let mut init = json!({"id":1,"method":"initialize"});
        protocol.tui_request(&mut init, &mut session).unwrap();
        protocol
            .backend_message(
                &mut json!({"id":init["id"],"result":{"userAgent":"codex"}}),
                &mut session,
            )
            .unwrap();
        assert!(
            protocol
                .tui_request(&mut json!({"method":"initialized"}), &mut session)
                .unwrap()
                .is_none()
        );
        let reply = protocol
            .tui_request(&mut json!({"id":3,"method":"initialize"}), &mut session)
            .unwrap()
            .unwrap();
        assert_eq!(reply["result"]["userAgent"], "codex");
        assert_eq!(reply["id"], 3);
        assert_eq!(
            protocol.tui_request(&mut json!({"method":"initialized"}), &mut session).unwrap(),
            Some(Value::Null)
        );
    }

    #[test]
    fn simultaneous_thread_creation_and_switching_cannot_rebind_a_session() {
        let mut protocol = Protocol::default();
        let mut session = session();
        protocol.tui_request(&mut json!({"id":1,"method":"thread/start"}), &mut session).unwrap();
        let reply = protocol
            .tui_request(&mut json!({"id":2,"method":"thread/start"}), &mut session)
            .unwrap()
            .unwrap();
        assert_eq!(reply["error"]["code"], proto::CONFLICT);
        protocol.pending.clear();
        session.native_thread_id = Some("main".into());
        let reply = protocol
            .tui_request(
                &mut json!({"id":3,"method":"thread/resume","params":{"threadId":"other"}}),
                &mut session,
            )
            .unwrap()
            .unwrap();
        assert_eq!(reply["error"]["code"], proto::CONFLICT);
    }

    #[test]
    fn approvals_resolve_without_thread_id_and_expire_at_turn_completion() {
        let mut protocol = Protocol::default();
        let mut session = session();
        session.native_thread_id = Some("main".into());
        protocol.backend_message(&mut json!({"method":"turn/started","params":{"threadId":"main","turn":{"id":"turn"}}}), &mut session).unwrap();
        let approval = |id| {
            json!({"id":id,"method":"item/commandExecution/requestApproval",
            "params":{"threadId":"main","turnId":"turn"}})
        };
        protocol.backend_message(&mut approval(20), &mut session).unwrap();
        protocol.backend_message(&mut approval(21), &mut session).unwrap();
        assert_eq!(session.state, SessionState::Blocked);
        protocol
            .backend_message(
                &mut json!({"method":"serverRequest/resolved","params":{"requestId":20}}),
                &mut session,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::Blocked);
        protocol
            .backend_message(
                &mut json!({"method":"serverRequest/resolved","params":{"requestId":21}}),
                &mut session,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::Working);
        protocol.backend_message(&mut approval(22), &mut session).unwrap();
        let mut reply = json!({"id":22,"result":{"decision":"decline"}});
        assert!(protocol.tui_request(&mut reply, &mut session).unwrap().is_none());
        assert_eq!(protocol.tui_request(&mut reply, &mut session).unwrap(), Some(Value::Null));
        protocol.backend_message(&mut approval(23), &mut session).unwrap();
        protocol.backend_message(&mut json!({"method":"turn/completed","params":{"threadId":"main","turn":{"id":"turn","status":"interrupted"}}}), &mut session).unwrap();
        assert_eq!(session.state, SessionState::Ready);
        assert!(session.turn_id.is_none());
        assert_eq!(
            protocol
                .tui_request(&mut json!({"id":23,"result":{"decision":"accept"}}), &mut session)
                .unwrap(),
            Some(Value::Null)
        );
    }

    #[test]
    fn auxiliary_turns_do_not_change_main_session_state() {
        let mut protocol = Protocol::default();
        let mut session = session();
        session.state = SessionState::Working;
        session.native_thread_id = Some("main".into());
        session.turn_id = Some("turn".into());
        protocol.backend_message(&mut json!({"method":"turn/completed","params":{"threadId":"auxiliary","turn":{"id":"other"}}}), &mut session).unwrap();
        assert_eq!(session.state, SessionState::Working);
        assert_eq!(session.turn_id.as_deref(), Some("turn"));
    }

    #[test]
    fn native_unsubscribe_keeps_the_managed_backends_subscription() {
        let mut protocol = Protocol::default();
        let mut session = session();
        session.native_thread_id = Some("main".into());
        let reply = protocol
            .tui_request(
                &mut json!({"id":8,"method":"thread/unsubscribe","params":{"threadId":"main"}}),
                &mut session,
            )
            .unwrap()
            .unwrap();
        assert_eq!(reply["result"]["status"], "unsubscribed");
        assert!(protocol.pending.is_empty());
    }

    #[test]
    fn file_approval_requires_complete_reviewable_changes() {
        let mut protocol = Protocol::default();
        let mut session = session();
        session.native_thread_id = Some("main".into());
        let request = |id| json!({"id":id,"method":"item/fileChange/requestApproval","params":{"threadId":"main","turnId":"turn","itemId":"file"}});
        protocol.backend_message(&mut request(10), &mut session).unwrap();
        assert!(!session.approvals.values().next().unwrap().controllable);
        protocol
            .backend_message(
                &mut json!({"method":"serverRequest/resolved","params":{"requestId":10}}),
                &mut session,
            )
            .unwrap();
        protocol.backend_message(&mut json!({"method":"item/started","params":{"threadId":"main","turnId":"turn","item":{"id":"file","type":"fileChange","changes":[{"path":"file.txt","diff":"+hello"}]}}}),&mut session).unwrap();
        protocol.backend_message(&mut request(11), &mut session).unwrap();
        let approval = session.approvals.values().next().unwrap();
        assert!(approval.controllable);
        assert_eq!(approval.details["changes"][0]["path"], "file.txt");
    }
}
