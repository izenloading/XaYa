use std::io::{self, BufReader, ErrorKind};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use xaya_agent_proto::{
    self as proto, InitializeParams, InitializeResult, Request, Response, ServerStatus,
    ShutdownResult,
};

use crate::Result;
use crate::events::Subscription;
use crate::runtime::Runtime;
use crate::sessions::Sessions;
use crate::wire::{IO_TIMEOUT, read_frame, response_limit, write_frame, write_frame_with_limit};

const MAX_CLIENTS: usize = 16;

pub fn serve(runtime: &Runtime, idle_timeout: Duration, stopping: Arc<AtomicBool>) -> Result<()> {
    serve_with_state(runtime, idle_timeout, stopping, None)
}

pub fn serve_with_state(
    runtime: &Runtime,
    idle_timeout: Duration,
    stopping: Arc<AtomicBool>,
    state_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    let (listener, _lease) = runtime.bind()?;
    let started = Instant::now();
    let store = crate::storage::Store::open(runtime, state_dir)?;
    let sessions = Arc::new(Sessions::open(Arc::clone(&store))?);
    // Codex backends live in their own hosts; join those that outlived the previous service.
    sessions.readopt();
    let checkpoints =
        Arc::new(crate::checkpoints::Checkpoints::open(Arc::clone(&store), Arc::clone(&sessions))?);
    let checks = Arc::new(crate::checks::Checks::open(
        Arc::clone(&store),
        Arc::clone(&sessions),
        Arc::clone(&checkpoints),
    )?);
    let tasks = Arc::new(crate::tasks::Tasks::open(Arc::clone(&store), Arc::clone(&sessions))?);
    let reviews = Arc::new(crate::reviews::Reviews::open(
        Arc::clone(&store),
        Arc::clone(&sessions),
        Arc::clone(&checkpoints),
        Arc::clone(&checks),
        Arc::clone(&tasks),
    )?);
    let landings = Arc::new(crate::landings::Landings::open(
        store,
        Arc::clone(&sessions),
        Arc::clone(&checkpoints),
        Arc::clone(&checks),
    )?);
    // The queue worker calls the same local dispatch operation as a manual task start; it
    // never consumes an IPC client slot or holds a transaction while waiting for an engine.
    let queue_worker = QueueWorker::spawn(
        runtime.clone(),
        Arc::clone(&sessions),
        Arc::clone(&tasks),
        Arc::clone(&reviews),
        Arc::clone(&landings),
        Arc::clone(&stopping),
    )?;
    let drive_worker = DriveWorker::spawn(
        runtime.clone(),
        Locals {
            sessions: Arc::clone(&sessions),
            tasks: Arc::clone(&tasks),
            reviews: Arc::clone(&reviews),
            landings: Arc::clone(&landings),
            checks: Arc::clone(&checks),
            checkpoints: Arc::clone(&checkpoints),
        },
        Arc::clone(&stopping),
    )?;
    let activity = Arc::new(AtomicBool::new(false));
    let mut last_activity = started;
    let mut clients = Vec::<thread::JoinHandle<()>>::new();
    while !stopping.load(Ordering::Acquire) {
        let mut index = 0;
        while index < clients.len() {
            if clients[index].is_finished() {
                let _ = clients.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        if activity.swap(false, Ordering::AcqRel) {
            last_activity = Instant::now();
        }
        if last_activity.elapsed() >= idle_timeout
            && clients.is_empty()
            && sessions.active_count() == 0
            && !checkpoints.active()
            && !checks.active()
            && !landings.active()
            && !reviews.active()
            && !tasks.has_pending_queue()
            && !tasks.has_pending_drive()
        {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) if clients.len() < MAX_CLIENTS => {
                let stopping = Arc::clone(&stopping);
                let activity = Arc::clone(&activity);
                let sessions = Arc::clone(&sessions);
                let checkpoints = Arc::clone(&checkpoints);
                let checks = Arc::clone(&checks);
                let landings = Arc::clone(&landings);
                let reviews = Arc::clone(&reviews);
                let tasks = Arc::clone(&tasks);
                let runtime = runtime.clone();
                clients.push(thread::spawn(move || {
                    // Disconnected/malformed clients do not terminate the listener.
                    let _ = connection(
                        stream,
                        started,
                        &stopping,
                        &activity,
                        &runtime,
                        &sessions,
                        (&checkpoints, &checks, &landings, &reviews, &tasks),
                    );
                }));
            },
            Ok(_) => (), // Bound resource use; the busy connection is closed here.
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10))
            },
            Err(error) if error.kind() == ErrorKind::Interrupted => (),
            Err(error) => return Err(error.into()),
        }
    }
    // Existing reads have a total deadline. Keep the lock until all readers have exited.
    for client in clients {
        let _ = client.join();
    }
    drop(drive_worker);
    drop(queue_worker);
    drop(landings);
    drop(reviews);
    drop(checks);
    drop(checkpoints);
    sessions.stop_all();
    Ok(())
}

/// Dropped before the service stops engines, including on an early listener error.
struct QueueWorker {
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl QueueWorker {
    fn spawn(
        runtime: Runtime,
        sessions: Arc<Sessions>,
        tasks: Arc<crate::tasks::Tasks>,
        reviews: Arc<crate::reviews::Reviews>,
        landings: Arc<crate::landings::Landings>,
        service_stopping: Arc<AtomicBool>,
    ) -> Result<Self> {
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let thread = thread::Builder::new().name("task-queue".into()).spawn(move || {
            while !worker_stopping.load(Ordering::Acquire)
                && !service_stopping.load(Ordering::Acquire)
            {
                for queued in tasks.pending_queue() {
                    if worker_stopping.load(Ordering::Acquire)
                        || service_stopping.load(Ordering::Acquire)
                    {
                        break;
                    }
                    let result = start_task(
                        &runtime,
                        &sessions,
                        &tasks,
                        &reviews,
                        &landings,
                        &service_stopping,
                        proto::TaskStartParams {
                            task_id: queued.task_id.clone(),
                            request_id: queued.request_id.clone(),
                            claude: queued.claude.clone(),
                            codex: queued.codex.clone(),
                            workspace: queued.workspace.clone(),
                        },
                    );
                    // Shutdown retains queued work as-is. A persisted claim is reconciled
                    // on the next start, never replaced with a fresh request ID.
                    if service_stopping.load(Ordering::Acquire)
                        || worker_stopping.load(Ordering::Acquire)
                    {
                        break;
                    }
                    let (state, reason, session_id) = match result {
                        Ok(session) => (proto::TaskQueueState::Dispatched, None, Some(session.id)),
                        Err(error) => {
                            let Ok(current) = tasks.get(&queued.task_id) else { continue };
                            // Cancellation and close may have won after our scan. Their
                            // result is final, and must not be overwritten by this worker.
                            if current.status != proto::TaskStatus::Open
                                || !current.queue.as_ref().is_some_and(|queue| {
                                    queue.request_id == queued.request_id
                                        && matches!(
                                            queue.state,
                                            proto::TaskQueueState::Queued
                                                | proto::TaskQueueState::Waiting
                                        )
                                })
                            {
                                continue;
                            }
                            let waiting = current.launch.is_none() && queue_waits_for(&error);
                            let state = if waiting {
                                proto::TaskQueueState::Waiting
                            } else {
                                proto::TaskQueueState::NeedsAttention
                            };
                            (
                                state,
                                Some(error.to_string()),
                                current.launch.map(|launch| launch.session_id),
                            )
                        },
                    };
                    if let Err(error) = tasks.queue_update(
                        &queued.task_id,
                        &queued.request_id,
                        state,
                        reason,
                        session_id,
                    ) {
                        // A cancelled/closed queue is not a storage failure. Any other
                        // failure stops this worker before it could dispatch further work.
                        if tasks.get(&queued.task_id).is_ok_and(|task| {
                            task.status != proto::TaskStatus::Open
                                || task.queue.as_ref().is_some_and(|queue| {
                                    matches!(
                                        queue.state,
                                        proto::TaskQueueState::Cancelled
                                            | proto::TaskQueueState::Dispatched
                                            | proto::TaskQueueState::NeedsAttention
                                    )
                                })
                        }) {
                            continue;
                        }
                        eprintln!("task queue stopped after persistent state failure: {error}");
                        return;
                    }
                }
                // Small sleeps keep shutdown responsive without an IPC self-connection.
                for _ in 0..10 {
                    if worker_stopping.load(Ordering::Acquire)
                        || service_stopping.load(Ordering::Acquire)
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        })?;
        Ok(Self { stopping, thread: Some(thread) })
    }
}

impl Drop for QueueWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The services a background operation is dispatched to.
#[derive(Clone)]
struct Locals {
    sessions: Arc<Sessions>,
    tasks: Arc<crate::tasks::Tasks>,
    reviews: Arc<crate::reviews::Reviews>,
    landings: Arc<crate::landings::Landings>,
    checks: Arc<crate::checks::Checks>,
    checkpoints: Arc<crate::checkpoints::Checkpoints>,
}

/// Dispatches an operation exactly as a client request would be, without a socket.
struct Local<'a> {
    runtime: &'a Runtime,
    services: &'a Locals,
    stopping: &'a AtomicBool,
    next_id: i64,
}

impl crate::drive::Call for Local<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id: proto::RequestId::Number(self.next_id),
            method: method.into(),
            params,
        };
        let s = self.services;
        let response = if method.starts_with("review.") {
            dispatch_review(request, &s.reviews)?
        } else if method.starts_with("task.") {
            dispatch_task(
                request,
                self.runtime,
                &s.tasks,
                &s.reviews,
                &s.landings,
                &s.checks,
                &s.sessions,
                self.stopping,
            )?
        } else if method.starts_with("check.") || method.starts_with("project.") {
            dispatch_check(request, &s.checks)?
        } else if method.starts_with("checkpoint.") {
            dispatch_checkpoint(request, &s.checkpoints)?
        } else if method.starts_with("land.") {
            dispatch_land(request, &s.landings)?
        } else if method.starts_with("session.") || method == proto::EVENTS_HISTORY {
            dispatch_session(request, self.runtime, &s.sessions)?
        } else {
            return Err(crate::Error::Protocol("unsupported background operation"));
        };
        match response {
            proto::Response::Success(success) => Ok(success.result),
            proto::Response::Failure(failure) => {
                Err(crate::Error::Rpc { code: failure.error.code, message: failure.error.message })
            },
        }
    }
}

/// Takes the steps of every review-and-fix cycle the service was asked to drive. Separate from
/// the queue worker: a step may wait on an engine (a resume), and must not hold up launches.
struct DriveWorker {
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

/// Steps taken for one drive per pass; a wait or an outcome ends the pass earlier.
const DRIVE_STEPS_PER_PASS: usize = 32;

impl DriveWorker {
    fn spawn(
        runtime: Runtime,
        services: Locals,
        service_stopping: Arc<AtomicBool>,
    ) -> Result<Self> {
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let thread = thread::Builder::new().name("task-drive".into()).spawn(move || {
            let halted = || {
                worker_stopping.load(Ordering::Acquire) || service_stopping.load(Ordering::Acquire)
            };
            while !halted() {
                for drive in services.tasks.pending_drives() {
                    if halted() {
                        break;
                    }
                    if let Err(error) =
                        drive_pass(&runtime, &services, &service_stopping, &halted, drive)
                    {
                        // The drive's state could not be recorded: taking further steps could
                        // repeat work without a record. Stop until the service restarts.
                        eprintln!("task drive stopped after persistent state failure: {error}");
                        return;
                    }
                }
                for (task_id, landing) in services.tasks.pending_landings() {
                    if halted() {
                        break;
                    }
                    if let Err(error) = landing_pass(
                        &runtime,
                        &services,
                        &service_stopping,
                        &halted,
                        &task_id,
                        landing,
                    ) {
                        eprintln!("task landing stopped after persistent state failure: {error}");
                        return;
                    }
                }
                for _ in 0..10 {
                    if halted() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        })?;
        Ok(Self { stopping, thread: Some(thread) })
    }
}

impl Drop for DriveWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Take steps of one drive until it waits or ends. Only a failure to record a step is an error.
fn drive_pass(
    runtime: &Runtime,
    services: &Locals,
    stopping: &AtomicBool,
    halted: &dyn Fn() -> bool,
    drive: proto::TaskDriveRecord,
) -> Result<()> {
    use crate::drive::{Outcome, Plan, State, Step};
    use proto::TaskDriveState as Drive;
    let tasks = &services.tasks;
    let (task_id, request_id) = (&drive.params.task_id, &drive.params.request_id);
    let record = |state: Drive, reason: Option<String>, session: Option<&str>, progress| {
        tasks.drive_update(task_id, request_id, |drive| {
            drive.state = state;
            drive.reason = reason;
            if let Some(session) = session {
                drive.session_id = Some(session.into());
            }
            if let Some(progress) = progress {
                drive.progress = Some(progress);
            }
        })
    };
    // A foreground `task cycle` of the same task takes the same steps: never both at once.
    let Ok(_cycle) = runtime.lock_task_cycle(task_id) else {
        record(
            Drive::Waiting,
            Some("a foreground cycle is running for this task".into()),
            None,
            None,
        )?;
        return Ok(());
    };
    let task = match tasks.get(task_id) {
        Ok(task) => task,
        Err(error) => {
            record(Drive::Stopped, Some(error.to_string()), None, None)?;
            return Ok(());
        },
    };
    let session_id = match (&drive.params.session_id, task.sessions.as_slice()) {
        (Some(session), _) => session.clone(),
        (None, [only]) => only.clone(),
        (None, []) => {
            record(Drive::Waiting, Some("the task has no session yet".into()), None, None)?;
            return Ok(());
        },
        (None, _) => {
            let reason = "the task has several sessions; drive it with an explicit session";
            record(Drive::Stopped, Some(reason.into()), None, None)?;
            return Ok(());
        },
    };
    let mut state = match drive.progress.map(serde_json::from_value::<State>) {
        None => State::new(crate::drive::now_ms()),
        Some(Ok(state)) => state,
        Some(Err(_)) => {
            let reason = "the drive's recorded progress is unreadable";
            record(Drive::Stopped, Some(reason.into()), Some(&session_id), None)?;
            return Ok(());
        },
    };
    let plan = Plan {
        task_id: task_id.clone(),
        session_id: session_id.clone(),
        reviewer: drive.params.reviewer.clone(),
        second_reviewer: drive.params.second_reviewer.clone(),
        timeout_secs: drive.params.timeout_secs,
        background: true,
    };
    let mut local = Local { runtime, services, stopping, next_id: 0 };
    for _ in 0..DRIVE_STEPS_PER_PASS {
        if halted() {
            return Ok(());
        }
        let result = crate::drive::step(&mut local, &plan, &mut state, crate::drive::now_ms());
        if halted() {
            // Shutdown may have failed the step; the phase recorded so far is retaken later.
            return Ok(());
        }
        let (next, reason) = match result {
            Ok(Step::Advanced) => (Drive::Driving, None),
            Ok(Step::Wait(reason)) => (Drive::Waiting, Some(reason)),
            Ok(Step::Finished(Outcome::Approved, reason)) => (Drive::Approved, reason),
            Ok(Step::Finished(Outcome::NeedsAttention, reason)) => (Drive::NeedsAttention, reason),
            Ok(Step::Finished(Outcome::Stopped, reason)) => (Drive::Stopped, reason),
            Err(error) => (Drive::Stopped, Some(error.to_string())),
        };
        let progress = serde_json::to_value(&state)?;
        if !record(next, reason, Some(&session_id), Some(progress))? || next != Drive::Driving {
            return Ok(());
        }
    }
    Ok(())
}

/// Take steps of one landing until it waits or ends. Only a failure to record a step is an
/// error; every other failure is recorded for a person.
fn landing_pass(
    runtime: &Runtime,
    services: &Locals,
    stopping: &AtomicBool,
    halted: &dyn Fn() -> bool,
    task_id: &str,
    mut landing: proto::TaskLandingRecord,
) -> Result<()> {
    use crate::task_landing::Step;
    use proto::TaskLandingState as State;
    let mut local = Local { runtime, services, stopping, next_id: 0 };
    if landing.state == State::Landed {
        return workspace_pass(&mut local, halted, task_id, &landing);
    }
    // Previewing, then verifying or landing: never more steps than that in one pass.
    for _ in 0..4 {
        if halted() {
            return Ok(());
        }
        let result = crate::task_landing::step(&mut local, task_id, &landing);
        if halted() {
            return Ok(()); // Shutdown may have failed the step; it is retaken later.
        }
        let (state, reason, preview) = match result {
            Ok(Step::Previewed(id)) => (State::Landing, None, Some(id)),
            Ok(Step::Wait(reason)) => (State::Landing, Some(reason), None),
            Ok(Step::Landed) => (State::Landed, None, None),
            Ok(Step::Failed(reason)) => (State::Failed, Some(reason), None),
            Err(error) => (State::Failed, Some(error.to_string()), None),
        };
        let advanced = preview.is_some();
        let request_id = landing.request_id.clone();
        let mut next = None;
        let recorded = services.tasks.landing_update(task_id, &request_id, |record| {
            record.state = state;
            record.reason = reason;
            if let Some(preview) = preview {
                record.preview_id = Some(preview);
            }
            next = Some(record.clone());
        })?;
        landing = next.unwrap_or(landing);
        if !recorded || !advanced {
            return Ok(());
        }
    }
    Ok(())
}

/// Clean up a landed task's workspace: record the intent, delete it, record the outcome. A
/// removal cut short by shutdown stays `removing` and is finished on the next pass.
fn workspace_pass(
    local: &mut Local<'_>,
    halted: &dyn Fn() -> bool,
    task_id: &str,
    landing: &proto::TaskLandingRecord,
) -> Result<()> {
    use proto::TaskWorkspaceOutcome as Outcome;
    let tasks = &local.services.tasks;
    let request_id = &landing.request_id;
    let stopping = local.stopping;
    let mut cleanup = match &landing.workspace {
        Some(cleanup) if cleanup.outcome == Outcome::Removing => cleanup.clone(),
        _ => {
            crate::task_landing::inspect(local, task_id, landing, stopping, crate::drive::now_ms())
        },
    };
    if halted() {
        return Ok(()); // Shutdown may have failed a step; it is retaken later.
    }
    if cleanup.outcome == Outcome::Removing {
        if !tasks.workspace_update(task_id, request_id, cleanup.clone())? {
            return Ok(());
        }
        let project = tasks.get(task_id)?.project.unwrap_or_default();
        let path = cleanup.path.clone().unwrap_or_default();
        let removed = crate::task_landing::remove(&project, &path, stopping);
        if halted() {
            return Ok(());
        }
        cleanup.at_ms = crate::drive::now_ms();
        match removed {
            Ok(()) => cleanup.outcome = Outcome::Removed,
            Err(error) => {
                cleanup.outcome = Outcome::Kept;
                cleanup.reason = Some(format!("removing it failed: {error}"));
            },
        }
    }
    tasks.workspace_update(task_id, request_id, cleanup)?;
    Ok(())
}

/// Recheck only pre-dispatch gates known to be safe to wait for. Arbitrary engine failures
/// and ambiguous launch results always need a person; they never become automatic retries.
fn queue_waits_for(error: &crate::Error) -> bool {
    let crate::Error::Rpc { code: proto::CONFLICT, message } = error else { return false };
    message.starts_with("project task capacity is occupied (limit ")
        || message == "another task launch is in progress for this project"
        || message == "another launch operation is in progress for this task"
        || (message.starts_with("prerequisite ")
            && [" is not satisfied in the launch checkout: ", " is not satisfied in base branch "]
                .iter()
                .any(|place| message.contains(place))
            && !message.contains("\"kind\":\"abandoned\"")
            && !message.contains("\"kind\":\"multiple_landing_targets\""))
}

/// The engine's working directory.
fn engine_cwd(engine: &mut crate::storage::EngineParams) -> &mut String {
    match engine {
        crate::storage::EngineParams::Claude(claude) => &mut claude.cwd,
        crate::storage::EngineParams::Codex(codex) => &mut codex.cwd,
    }
}

fn validate_engine(engine: &crate::storage::EngineParams) -> Result<()> {
    match engine {
        crate::storage::EngineParams::Claude(claude) => Sessions::validate_claude_start(claude),
        crate::storage::EngineParams::Codex(codex) => Sessions::validate_codex_start(codex),
    }
}

/// Point the launch at the task's own workspace, and check everything else about it before
/// that workspace exists.
fn workspace_launch(
    task: &proto::Task,
    engine: &mut crate::storage::EngineParams,
    workspace: &proto::TaskWorkspace,
) -> Result<String> {
    crate::workspaces::validate_base(workspace)?;
    let project = task.project.as_deref().ok_or_else(|| crate::Error::Rpc {
        code: proto::INVALID_PARAMS,
        message: "a task workspace requires the task's explicitly assigned project".into(),
    })?;
    let path = crate::workspaces::path(project, &task.id)?;
    let mut stand_in = engine.clone();
    *engine_cwd(&mut stand_in) = crate::workspaces::stand_in(&path)?;
    validate_engine(&stand_in)?;
    *engine_cwd(engine) = path.clone();
    Ok(path)
}

/// Create a task from one request, queued for a workspace of its own and driven to review, as
/// one durable record. A repeat of the request returns that task without looking at Git again.
fn delegate(
    tasks: &crate::tasks::Tasks,
    stopping: &AtomicBool,
    params: proto::TaskDelegateParams,
) -> Result<proto::Task> {
    let input_hash = crate::tasks::sha256(&serde_json::to_string(&params)?);
    if let Some(task) = tasks.delegated(&params.request_id, &input_hash)? {
        return Ok(task);
    }
    let invalid =
        |message: &str| crate::Error::Rpc { code: proto::INVALID_PARAMS, message: message.into() };
    if !(1..=3600).contains(&params.timeout_secs) {
        return Err(invalid("timeout_secs must be 1..=3600"));
    }
    let base_branch = match &params.base_branch {
        Some(branch) => branch.clone(),
        None => crate::workspaces::current_branch(&params.cwd, stopping)?,
    };
    let workspace = proto::TaskWorkspace { base_branch };
    let mut task = tasks.draft(proto::TaskCreateParams {
        project: Some(params.cwd.clone()),
        depends_on: Vec::new(),
        title: crate::tasks::title_of(&params.goal),
        goal: params.goal.trim().to_owned(),
        constraints: None,
        criteria: vec![crate::tasks::DELEGATED_CRITERION.into()],
        sessions: Vec::new(),
        fix_budget: None,
        dual_review: false,
    })?;
    let prompt = crate::tasks::prompt(&task);
    let implementer = params.implementer;
    let mut engine = match implementer.engine {
        proto::ReviewEngine::Claude => {
            crate::storage::EngineParams::Claude(proto::StartClaudeParams {
                cwd: String::new(),
                executable: implementer.executable,
                config_dir: implementer.config_dir,
                prompt,
            })
        },
        proto::ReviewEngine::Codex => {
            crate::storage::EngineParams::Codex(proto::StartSessionParams {
                cwd: String::new(),
                executable: implementer.executable,
                codex_home: implementer
                    .config_dir
                    .ok_or_else(|| invalid("a Codex implementer needs its CODEX_HOME"))?,
                // The workspace is the task's own: the implementer has to change it.
                sandbox: proto::Sandbox::WorkspaceWrite,
                prompt: Some(prompt),
            })
        },
    };
    workspace_launch(&task, &mut engine, &workspace)?;
    let (claude, codex) = match engine {
        crate::storage::EngineParams::Claude(claude) => (Some(claude), None),
        crate::storage::EngineParams::Codex(codex) => (None, Some(codex)),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    task.queue = Some(proto::TaskQueueRecord {
        task_id: task.id.clone(),
        request_id: params.request_id.clone(),
        workspace: Some(workspace),
        claude,
        codex,
        state: proto::TaskQueueState::Queued,
        reason: None,
        session_id: None,
        enqueued_at_ms: now,
    });
    task.drive = Some(proto::TaskDriveRecord {
        params: proto::TaskDriveParams {
            task_id: task.id.clone(),
            request_id: params.request_id.clone(),
            session_id: None,
            reviewer: params.reviewer,
            second_reviewer: None,
            timeout_secs: params.timeout_secs,
        },
        state: proto::TaskDriveState::Driving,
        reason: None,
        session_id: None,
        progress: None,
        started_at_ms: now,
        updated_at_ms: now,
    });
    task.delegation = Some(proto::TaskDelegation { request_id: params.request_id, input_hash });
    tasks.insert_delegated(task)
}

/// All task dispatch paths share the same durable claim and workspace gates.
fn start_task(
    runtime: &Runtime,
    sessions: &Sessions,
    tasks: &crate::tasks::Tasks,
    reviews: &crate::reviews::Reviews,
    landings: &crate::landings::Landings,
    stopping: &AtomicBool,
    params: proto::TaskStartParams,
) -> Result<proto::Session> {
    if stopping.load(Ordering::Acquire) {
        return Err(crate::Error::Protocol("service is stopping"));
    }
    let _launch_guard = runtime.lock_task_launch(&params.task_id)?;
    let task = tasks.get(&params.task_id)?;
    use crate::storage::EngineParams;
    let mut engine = match (params.claude, params.codex) {
        (Some(claude), None) => EngineParams::Claude(claude),
        (None, Some(codex)) => EngineParams::Codex(codex),
        _ => {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_PARAMS,
                message: "task start requires exactly one engine".into(),
            });
        },
    };
    let prefix = match &engine {
        EngineParams::Claude(_) => "claude-",
        EngineParams::Codex(_) => "codex-",
    };
    let workspace = match &params.workspace {
        Some(workspace) if task.launch.is_none() => {
            Some((workspace_launch(&task, &mut engine, workspace)?, workspace))
        },
        Some(_) => {
            // Only the claim's hash is compared on a repeat: replace the cwd the same way.
            if let Some(project) = &task.project {
                *engine_cwd(&mut engine) = crate::workspaces::path(project, &task.id)?;
            }
            None
        },
        None => {
            if task.launch.is_none() {
                validate_engine(&engine)?;
            }
            None
        },
    };
    let input_hash = crate::tasks::launch_hash(&engine, params.workspace.as_ref())?;
    let _project_guard = if task.launch.is_none() {
        let project = match &workspace {
            Some(_) => task.project.clone().unwrap_or_default(),
            None => crate::tasks::repository(engine_cwd(&mut engine), stopping)?,
        };
        if task.project.as_ref() != Some(&project) {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_PARAMS,
                message: "task start requires an explicitly assigned project".into(),
            });
        }
        // Keep the project claim serialized through session creation. Once the
        // lock is released, the durable launch or its session occupies the slot.
        let guard = runtime.lock_project_launch(&project)?;
        let capacity = tasks.task_capacity(&project)?;
        if project_capacity(&project, tasks, sessions, stopping).occupied >= capacity {
            return Err(crate::Error::Rpc {
                code: proto::CONFLICT,
                message: format!(
                    "project task capacity is occupied (limit {capacity}); stop or reconcile an \
                     existing session or launch"
                ),
            });
        }
        // A new workspace starts at the base branch's tip, so prerequisites are checked there
        // before anything is created; an adopted one is checked at its own HEAD.
        let (checkout, revision, place) = match &workspace {
            Some((path, base)) if !std::path::Path::new(path).exists() => (
                project.clone(),
                crate::workspaces::base_tip(&project, base, stopping)?,
                format!("base branch {}", base.base_branch),
            ),
            Some((path, _)) => {
                // Run nothing inside a path before it is proven to be this task's workspace.
                crate::workspaces::prepare(&project, path, "HEAD", stopping)?;
                (path.clone(), "HEAD".into(), "the task workspace".into())
            },
            None => (engine_cwd(&mut engine).clone(), "HEAD".into(), "the launch checkout".into()),
        };
        // A task without a session has no checkout for dependency_snapshot to
        // inspect. Check the proposed launch checkout before claiming anything.
        for dependency in dependency_snapshot(&task, tasks, reviews, landings, sessions, stopping)?
        {
            let included = match &dependency.state {
                proto::DependencyState::AwaitingCheckout { commit } => {
                    crate::checkpoints::Git::new(std::path::Path::new(&checkout), stopping)
                        .output(&["merge-base", "--is-ancestor", commit, &revision], None)
                        .is_ok_and(|output| output.status.success())
                },
                proto::DependencyState::Included { .. } => true,
                _ => false,
            };
            if !included {
                return Err(crate::Error::Rpc {
                    code: proto::CONFLICT,
                    message: format!(
                        "prerequisite {} is not satisfied in {place}: {}",
                        dependency.task_id,
                        serde_json::to_string(&dependency.state)?
                    ),
                });
            }
        }
        if let Some((path, _)) = &workspace {
            crate::workspaces::prepare(&project, path, &revision, stopping)?;
            crate::projects::inherit_trust(tasks.store(), &project, path, stopping)?;
            crate::faults::crash_point("task.after_workspace");
            validate_engine(&engine)?;
        }
        Some(guard)
    } else {
        None
    };
    let directory = tempfile::Builder::new().prefix(prefix).tempdir_in(runtime.directory())?;
    let id = directory
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(crate::Error::Protocol("invalid launch directory"))?;
    if stopping.load(Ordering::Acquire) {
        return Err(crate::Error::Protocol("service is stopping"));
    }
    let (launch, fresh) = tasks.claim_launch(&task.id, &params.request_id, id, input_hash)?;
    if fresh {
        crate::faults::crash_point("task.after_claim");
        let mark_started = || {
            if stopping.load(Ordering::Acquire) {
                return Err(crate::Error::Protocol("service is stopping"));
            }
            tasks.mark_launch_started(&task.id, &launch)
        };
        match engine {
            EngineParams::Claude(claude) => {
                sessions.start_claude_in(claude, directory, mark_started)
            },
            EngineParams::Codex(codex) => sessions.start_codex_in(codex, directory, mark_started),
        }
    } else {
        sessions.get(&launch.session_id).map_err(|_| crate::Error::Rpc {
            code: proto::CONFLICT,
            message: format!("launch requires reconciliation for session {}", launch.session_id),
        })
    }
}

fn connection(
    stream: UnixStream,
    started: Instant,
    stopping: &AtomicBool,
    activity: &AtomicBool,
    runtime: &Runtime,
    sessions: &Sessions,
    verification: (
        &crate::checkpoints::Checkpoints,
        &crate::checks::Checks,
        &crate::landings::Landings,
        &crate::reviews::Reviews,
        &crate::tasks::Tasks,
    ),
) -> Result<()> {
    let (checkpoints, checks, landings, reviews, tasks) = verification;
    let mut reader = BufReader::new(stream);
    let mut initialized = false;
    while !stopping.load(Ordering::Acquire) {
        let bytes = read_frame(&mut reader, IO_TIMEOUT)?;
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                write_frame(
                    reader.get_mut(),
                    &Response::failure(None, proto::PARSE_ERROR, "invalid JSON"),
                )?;
                if !initialized {
                    break;
                }
                continue;
            },
        };
        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(_) => {
                write_frame(
                    reader.get_mut(),
                    &Response::failure(None, proto::INVALID_REQUEST, "invalid request"),
                )?;
                if !initialized {
                    break;
                }
                continue;
            },
        };
        if initialized && request.method == proto::ATTACH_CLAIM && request.jsonrpc == "2.0" {
            let result = serde_json::from_value::<proto::SessionParams>(request.params)
                .map_err(|_| crate::Error::Protocol("session_id required"))
                .and_then(|params| sessions.claim_attach(&params.session_id));
            match result {
                Ok((info, _lease)) => {
                    write_frame(reader.get_mut(), &Response::success(request.id, info)?)?;
                    // read_frame/write_frame use nonblocking sockets. This lease waits on EOF;
                    // restore blocking reads so its timeout does not turn into a busy loop.
                    reader.get_mut().set_nonblocking(false)?;
                    reader.get_mut().set_read_timeout(Some(Duration::from_millis(200)))?;
                    use std::io::Read;
                    while !stopping.load(Ordering::Acquire) {
                        let mut byte = [0];
                        match reader.read(&mut byte) {
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    ErrorKind::WouldBlock
                                        | ErrorKind::TimedOut
                                        | ErrorKind::Interrupted
                                ) => {},
                            _ => break,
                        }
                    }
                    return Ok(());
                },
                Err(error) => {
                    write_frame(reader.get_mut(), &rpc_failure(request.id, error))?;
                    continue;
                },
            }
        }
        if initialized && request.method == proto::BOARD_WATCH && request.jsonrpc == "2.0" {
            match sessions.watch_board() {
                Ok(slot) => {
                    // Read the version before the board: a change in between is pushed again.
                    let seen = crate::events::BOARD.version();
                    write_frame(
                        reader.get_mut(),
                        &Response::success(request.id, sessions.board())?,
                    )?;
                    return stream_board(reader.get_mut(), sessions, slot, seen, stopping);
                },
                Err(error) => {
                    write_frame(reader.get_mut(), &rpc_failure(request.id, error))?;
                    continue;
                },
            }
        }
        if initialized && request.method == proto::EVENTS_SUBSCRIBE && request.jsonrpc == "2.0" {
            let result = serde_json::from_value::<proto::SubscribeParams>(request.params)
                .map_err(|_| crate::Error::Rpc {
                    code: proto::INVALID_PARAMS,
                    message: "session_id required".into(),
                })
                .and_then(|params| sessions.subscribe(&params.session_id, params.after.as_ref()));
            match result {
                Ok((subscription, replay)) => {
                    write_frame(
                        reader.get_mut(),
                        &Response::success(request.id, &subscription.snapshot)?,
                    )?;
                    return stream_events(reader.get_mut(), subscription, replay, stopping);
                },
                Err(error) => {
                    write_frame(reader.get_mut(), &rpc_failure(request.id, error))?;
                    continue;
                },
            }
        }
        let limit = response_limit(&request.method);
        let (response, shutdown) =
            if initialized && request.method == proto::TASK_START && request.jsonrpc == "2.0" {
                let result = (|| -> Result<proto::Session> {
                    let params: proto::TaskStartParams = serde_json::from_value(request.params)
                        .map_err(|_| crate::Error::Rpc {
                            code: proto::INVALID_PARAMS,
                            message: "invalid task launch parameters".into(),
                        })?;
                    start_task(runtime, sessions, tasks, reviews, landings, stopping, params)
                })();
                (
                    match result {
                        Ok(session) => Response::success(request.id, session)?,
                        Err(error) => rpc_failure(request.id, error),
                    },
                    false,
                )
            } else if initialized
                && request.method == proto::TASK_RESET_LAUNCH
                && request.jsonrpc == "2.0"
            {
                let result = serde_json::from_value(request.params)
                    .map_err(|_| crate::Error::Rpc {
                        code: proto::INVALID_PARAMS,
                        message: "invalid task launch reset parameters".into(),
                    })
                    .and_then(|params| tasks.reset_launch(runtime, params));
                (
                    match result {
                        Ok(task) => Response::success(request.id, task)?,
                        Err(error) => rpc_failure(request.id, error),
                    },
                    false,
                )
            } else if initialized && request.method.starts_with("land.") {
                (dispatch_land(request, landings)?, false)
            } else if initialized && request.method.starts_with("review.") {
                (dispatch_review(request, reviews)?, false)
            } else if initialized && request.method.starts_with("task.") {
                (
                    dispatch_task(
                        request, runtime, tasks, reviews, landings, checks, sessions, stopping,
                    )?,
                    false,
                )
            } else if initialized
                && (request.method.starts_with("check.") || request.method.starts_with("project."))
            {
                (dispatch_check(request, checks)?, false)
            } else if initialized && request.method.starts_with("checkpoint.") {
                (dispatch_checkpoint(request, checkpoints)?, false)
            } else if initialized
                && (request.method.starts_with("session.")
                    || request.method.starts_with("approval.")
                    || request.method == proto::BOARD_SNAPSHOT
                    || request.method == proto::EVENTS_HISTORY)
            {
                (dispatch_session(request, runtime, sessions)?, false)
            } else {
                dispatch(request, &mut initialized, started, sessions.active_count())?
            };
        if initialized {
            activity.store(true, Ordering::Release);
        }
        write_frame_with_limit(reader.get_mut(), &response, limit)?;
        if !initialized {
            break;
        }
        if shutdown {
            stopping.store(true, Ordering::Release);
            break;
        }
    }
    Ok(())
}

fn dispatch(
    request: Request,
    initialized: &mut bool,
    started: Instant,
    active_sessions: u32,
) -> Result<(Response, bool)> {
    let error = |code, message| (Response::failure(Some(request.id.clone()), code, message), false);
    if request.jsonrpc != "2.0" {
        return Ok(error(proto::INVALID_REQUEST, "JSON-RPC 2.0 required"));
    }
    if request.method == proto::INITIALIZE {
        if *initialized {
            return Ok(error(proto::INVALID_REQUEST, "already initialized"));
        }
        let params: InitializeParams = match serde_json::from_value(request.params.clone()) {
            Ok(params) => params,
            Err(_) => return Ok(error(proto::INVALID_PARAMS, "invalid initialization parameters")),
        };
        if params.protocol_version != proto::PROTOCOL_VERSION {
            return Ok(error(proto::VERSION_MISMATCH, "unsupported protocol version"));
        }
        if params.client_name.trim().is_empty()
            || params.client_name.len() > 64
            || params.client_name.chars().any(char::is_control)
        {
            return Ok(error(proto::INVALID_PARAMS, "invalid client name"));
        }
        *initialized = true;
        return Ok((
            Response::success(request.id, InitializeResult {
                protocol_version: proto::PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: [
                    proto::STATUS,
                    proto::SHUTDOWN,
                    proto::SESSION_START,
                    proto::CLAUDE_START,
                    proto::ATTACH_CLAIM,
                    proto::SESSION_LIST,
                    proto::SESSION_GET,
                    proto::SESSION_ATTACH_INFO,
                    proto::SESSION_STOP,
                    proto::SESSION_STOP_IDLE,
                    proto::SESSION_END_TOOLS,
                    proto::SESSION_SEND,
                    proto::SESSION_CANCEL,
                    proto::SESSION_COMMAND,
                    proto::APPROVAL_LIST,
                    proto::APPROVAL_RESPOND,
                    proto::EVENTS_SUBSCRIBE,
                    proto::EVENTS_HISTORY,
                    proto::BOARD_WATCH,
                    proto::SESSION_RESUME,
                    proto::BOARD_SNAPSHOT,
                    proto::CHECKPOINT_CREATE,
                    proto::CHECKPOINT_GET,
                    proto::CHECK_RUN,
                    proto::CHECK_GET,
                    proto::CHECK_CANCEL,
                    proto::CHECK_CLEANUP,
                    proto::CHECK_LOG,
                    proto::CHECK_PROJECT,
                    proto::CHECK_LATEST,
                    proto::CHECK_BIND,
                    proto::PROJECT_GET,
                    proto::PROJECT_TRUST,
                    proto::LAND_PREVIEW,
                    proto::LAND_GET,
                    proto::LAND_RECHECK,
                    proto::LAND_VERIFY,
                    proto::LAND_RUN,
                    proto::LAND_RECONCILE,
                    proto::LAND_RESUME,
                    proto::REVIEW_START,
                    proto::REVIEW_GET,
                    proto::REVIEW_CANCEL,
                    proto::REVIEW_LIST,
                    proto::REVIEW_SEND,
                    proto::REVIEW_DISPUTE,
                    proto::TASK_CONTINUE,
                    proto::TASK_CREATE,
                    proto::TASK_GET,
                    proto::TASK_LIST,
                    proto::TASK_DEPENDENCIES,
                    proto::TASK_BOARD,
                    proto::TASK_CAPACITY_GET,
                    proto::TASK_CAPACITY_SET,
                    proto::TASK_ENGINE_GET,
                    proto::TASK_ENGINE_SET,
                    proto::TASK_START,
                    proto::TASK_ENQUEUE,
                    proto::TASK_DELEGATE,
                    proto::TASK_LAND,
                    proto::TASK_ACCEPTANCE,
                    proto::TASK_REVISE,
                    proto::TASK_DEQUEUE,
                    proto::TASK_DRIVE,
                    proto::TASK_UNDRIVE,
                    proto::TASK_RESET_LAUNCH,
                    proto::TASK_CLOSE,
                    proto::TASK_FOR_SESSION,
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            })?,
            false,
        ));
    }
    if !*initialized {
        return Ok(error(proto::NOT_INITIALIZED, "initialize before sending requests"));
    }
    if !matches!(request.method.as_str(), proto::STATUS | proto::SHUTDOWN) {
        return Ok(error(proto::METHOD_NOT_FOUND, "unknown method"));
    }
    if !request.params.is_null()
        && !request.params.as_object().is_some_and(|params| params.is_empty())
    {
        return Ok(error(proto::INVALID_PARAMS, "method accepts no parameters"));
    }
    match request.method.as_str() {
        proto::STATUS => Ok((
            Response::success(request.id, ServerStatus {
                protocol_version: proto::PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
                uptime_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                active_sessions,
            })?,
            false,
        )),
        proto::SHUTDOWN => {
            Ok((Response::success(request.id, ShutdownResult { accepted: true })?, true))
        },
        _ => Err(io::Error::other("unreachable method dispatch").into()),
    }
}

fn dispatch_land(request: Request, landings: &crate::landings::Landings) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid landing parameters".into(),
        };
        match request.method.as_str() {
            proto::LAND_PREVIEW => Ok(serde_json::to_value(
                landings.preview(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::LAND_GET
            | proto::LAND_RECHECK
            | proto::LAND_VERIFY
            | proto::LAND_RUN
            | proto::LAND_RECONCILE
            | proto::LAND_RESUME => {
                let params: proto::LandParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                if request.method == proto::LAND_GET {
                    Ok(serde_json::to_value(landings.get(&params.preview_id)?)?)
                } else if request.method == proto::LAND_VERIFY {
                    Ok(serde_json::to_value(landings.verify(&params.preview_id)?)?)
                } else if request.method == proto::LAND_RECONCILE {
                    Ok(serde_json::to_value(landings.reconcile(&params.preview_id)?)?)
                } else if request.method == proto::LAND_RESUME {
                    Ok(serde_json::to_value(landings.resume(&params.preview_id)?)?)
                } else if request.method == proto::LAND_RUN {
                    Ok(serde_json::to_value(landings.run(&params.preview_id)?)?)
                } else {
                    Ok(serde_json::to_value(landings.recheck(&params.preview_id)?)?)
                }
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Ok(Response::success(request.id, value)?),
        Err(error) => Ok(rpc_failure(request.id, error)),
    }
}

fn dispatch_review(request: Request, reviews: &crate::reviews::Reviews) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid review parameters".into(),
        };
        match request.method.as_str() {
            proto::REVIEW_START => Ok(serde_json::to_value(
                reviews.start(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::REVIEW_DISPUTE => Ok(serde_json::to_value(
                reviews.dispute(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::REVIEW_SEND => Ok(serde_json::to_value(
                reviews.send(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::REVIEW_LIST => {
                let params: proto::ReviewListParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(
                    reviews.list(params.task_id.as_deref(), params.session_id.as_deref()),
                )?)
            },
            proto::REVIEW_GET | proto::REVIEW_CANCEL => {
                let params: proto::ReviewParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(if request.method == proto::REVIEW_GET {
                    reviews.get(&params.review_id)?
                } else {
                    reviews.cancel(&params.review_id)?
                })?)
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Ok(Response::success(request.id, value)?),
        Err(error) => Ok(rpc_failure(request.id, error)),
    }
}

struct ProjectCapacity {
    local: std::collections::BTreeMap<String, proto::SessionState>,
    occupied: usize,
}

/// Shared by the read-only board and task dispatch. A durable launch without a session is an
/// unresolved reservation even if its task was closed, unless a durable retirement proves
/// that its session ended before eviction. Failed proof reads keep the reservation occupied.
fn project_capacity(
    project: &str,
    tasks: &crate::tasks::Tasks,
    sessions: &Sessions,
    stopping: &AtomicBool,
) -> ProjectCapacity {
    let tasks = tasks.list();
    let assigned: std::collections::BTreeSet<_> = tasks
        .iter()
        .filter(|task| task.project.as_deref() == Some(project))
        .flat_map(|task| task.sessions.iter())
        .collect();
    let mut local = std::collections::BTreeMap::new();
    let mut known = std::collections::BTreeSet::new();
    let mut repositories = std::collections::BTreeMap::new();
    for session in sessions.list() {
        known.insert(session.id.clone());
        let repo = repositories
            .entry(session.cwd.clone())
            .or_insert_with(|| crate::tasks::repository(&session.cwd, stopping).ok());
        // Stored task ownership also retains capacity when a checkout has disappeared or
        // become unreadable; failing to resolve its cwd is not evidence that it stopped.
        if repo.as_deref() == Some(project) || assigned.contains(&session.id) {
            local.insert(session.id, session.state);
        }
    }
    let unresolved: std::collections::BTreeSet<_> = tasks
        .iter()
        .filter(|task| task.project.as_deref() == Some(project))
        .filter_map(|task| task.launch.as_ref())
        .filter(|launch| {
            !known.contains(&launch.session_id)
                && !sessions.retired(&launch.session_id).unwrap_or(false)
        })
        .map(|launch| &launch.session_id)
        .collect();
    let occupied = local
        .values()
        .filter(|state| state.is_active() || **state == proto::SessionState::Interrupted)
        .count()
        + unresolved.len();
    ProjectCapacity { local, occupied }
}

fn checkout_inclusion(
    task: &proto::Task,
    commit: &str,
    sessions: &Sessions,
    stopping: &AtomicBool,
) -> proto::DependencyState {
    use proto::DependencyState;
    if task.sessions.is_empty() {
        return DependencyState::AwaitingCheckout { commit: commit.into() };
    }
    for id in &task.sessions {
        let result = sessions.get(id).and_then(|session| {
            crate::checkpoints::Git::new(std::path::Path::new(&session.cwd), stopping)
                .output(&["merge-base", "--is-ancestor", commit, "HEAD"], None)
        });
        match result.ok().and_then(|output| output.status.code()) {
            Some(0) => (),
            Some(1) => {
                return DependencyState::CheckoutBehind {
                    session_id: id.clone(),
                    commit: commit.into(),
                };
            },
            _ => {
                return DependencyState::CheckoutUnknown {
                    session_id: id.clone(),
                    commit: commit.into(),
                };
            },
        }
    }
    DependencyState::Included { commit: commit.into() }
}

fn dependency_snapshot(
    task: &proto::Task,
    tasks: &crate::tasks::Tasks,
    reviews: &crate::reviews::Reviews,
    landings: &crate::landings::Landings,
    sessions: &Sessions,
    stopping: &AtomicBool,
) -> Result<Vec<proto::DependencySnapshot>> {
    let mut dependencies = Vec::new();
    for id in &task.depends_on {
        let prior = tasks.get(id)?;
        let state = if prior.status == proto::TaskStatus::Abandoned {
            proto::DependencyState::Abandoned
        } else {
            let latest = reviews.list(Some(id), None).into_iter().next();
            let commits = latest
                .filter(|r| {
                    r.status == proto::ReviewStatus::Completed
                        && r.verdict == Some(proto::ReviewVerdict::Approved)
                })
                .map(|r| landings.landed_commits(&r.checkpoint_id))
                .unwrap_or_default();
            match (commits.len(), commits.first()) {
                (0, _) => proto::DependencyState::AwaitingLanding,
                (1, Some(commit)) => checkout_inclusion(task, commit, sessions, stopping),
                _ => proto::DependencyState::MultipleLandingTargets,
            }
        };
        dependencies.push(proto::DependencySnapshot {
            task_id: prior.id,
            title: prior.title,
            state,
        });
    }
    Ok(dependencies)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_task(
    request: Request,
    runtime: &Runtime,
    tasks: &crate::tasks::Tasks,
    reviews: &crate::reviews::Reviews,
    landings: &crate::landings::Landings,
    checks: &crate::checks::Checks,
    sessions: &Sessions,
    stopping: &AtomicBool,
) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid task parameters".into(),
        };
        match request.method.as_str() {
            proto::TASK_CREATE => Ok(serde_json::to_value(
                tasks.create(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::TASK_ENQUEUE => {
                let mut params: proto::TaskEnqueueParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let _launch_guard = runtime.lock_task_launch(&params.task_id)?;
                let task = tasks.get(&params.task_id)?;
                // The service names a workspace's checkout; whatever the caller sent is
                // replaced, so a repeat compares equal to the stored authorization.
                if let (Some(workspace), Some(project)) = (&params.workspace, &task.project) {
                    let path = crate::workspaces::path(project, &task.id)?;
                    for cwd in [
                        params.claude.as_mut().map(|claude| &mut claude.cwd),
                        params.codex.as_mut().map(|codex| &mut codex.cwd),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        *cwd = path.clone();
                    }
                    if task.queue.is_none() {
                        crate::workspaces::validate_base(workspace)?;
                    }
                }
                // A repeated enqueue queries the durable authorization, even after its
                // checkout or executable disappeared. The task layer verifies exact input.
                if task.queue.is_none() {
                    if let Some(workspace) = params.workspace.clone() {
                        let mut engine = match (&params.claude, &params.codex) {
                            (Some(claude), None) => {
                                crate::storage::EngineParams::Claude(claude.clone())
                            },
                            (None, Some(codex)) if codex.prompt.is_some() => {
                                crate::storage::EngineParams::Codex(codex.clone())
                            },
                            _ => {
                                return Err(crate::Error::Rpc {
                                    code: proto::INVALID_PARAMS,
                                    message: "queue exactly one engine: Claude, or Codex with its \
                                              first message"
                                        .into(),
                                });
                            },
                        };
                        workspace_launch(&task, &mut engine, &workspace)?;
                        return Ok(serde_json::to_value(tasks.enqueue(params)?)?);
                    }
                    let cwd = match (&params.claude, &params.codex) {
                        (Some(claude), None) => {
                            Sessions::validate_claude_start(claude)?;
                            &claude.cwd
                        },
                        (None, Some(codex)) if codex.prompt.is_some() => {
                            Sessions::validate_codex_start(codex)?;
                            &codex.cwd
                        },
                        _ => {
                            return Err(crate::Error::Rpc {
                                code: proto::INVALID_PARAMS,
                                message: "queue exactly one engine: Claude, or Codex with its \
                                          first message"
                                    .into(),
                            });
                        },
                    };
                    let project = crate::tasks::repository(cwd, stopping)?;
                    if task.project.as_ref() != Some(&project) {
                        return Err(crate::Error::Rpc {
                            code: proto::INVALID_PARAMS,
                            message: "queue requires the task's explicitly assigned project".into(),
                        });
                    }
                }
                Ok(serde_json::to_value(tasks.enqueue(params)?)?)
            },
            proto::TASK_LAND => {
                let params: proto::TaskLandParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.land(params)?)?)
            },
            proto::TASK_REVISE => {
                let params: proto::TaskReviseParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let latest = reviews.list(Some(&params.task_id), None).into_iter().next();
                Ok(serde_json::to_value(tasks.revise(params, latest)?)?)
            },
            proto::TASK_ACCEPTANCE => {
                let params: proto::TaskParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(crate::acceptance::acceptance(
                    &params.task_id,
                    tasks,
                    reviews,
                    checks,
                    stopping,
                )?)?)
            },
            proto::TASK_DELEGATE => {
                let params: proto::TaskDelegateParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(delegate(tasks, stopping, params)?)?)
            },
            proto::TASK_DEQUEUE => {
                let params: proto::TaskDequeueParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let _launch_guard = runtime.lock_task_launch(&params.task_id)?;
                Ok(serde_json::to_value(tasks.dequeue(params)?)?)
            },
            proto::TASK_DRIVE => {
                let params: proto::TaskDriveParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.drive(params)?)?)
            },
            proto::TASK_UNDRIVE => {
                let params: proto::TaskUndriveParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.undrive(params)?)?)
            },
            proto::TASK_GET => {
                let params: proto::TaskParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.get(&params.task_id)?)?)
            },
            proto::TASK_DEPENDENCIES => {
                let params: proto::TaskParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let task = tasks.get(&params.task_id)?;
                Ok(serde_json::to_value(dependency_snapshot(
                    &task, tasks, reviews, landings, sessions, stopping,
                )?)?)
            },
            proto::TASK_CAPACITY_GET => {
                let params: proto::ProjectParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let project = crate::tasks::repository(&params.cwd, stopping)?;
                let capacity = tasks.task_capacity(&project)?;
                Ok(serde_json::to_value(proto::ProjectTaskCapacity { project, capacity })?)
            },
            proto::TASK_CAPACITY_SET => {
                let params: proto::TaskCapacityParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let project = crate::tasks::repository(&params.cwd, stopping)?;
                let _project_guard = runtime.lock_project_launch(&project)?;
                tasks.set_task_capacity(&project, params.capacity)?;
                Ok(serde_json::to_value(proto::ProjectTaskCapacity {
                    project,
                    capacity: params.capacity,
                })?)
            },
            method @ (proto::TASK_ENGINE_GET | proto::TASK_ENGINE_SET) => {
                let cwd = if method == proto::TASK_ENGINE_SET {
                    let params: proto::TaskEngineParams =
                        serde_json::from_value(request.params).map_err(|_| invalid())?;
                    let project = crate::tasks::repository(&params.cwd, stopping)?;
                    tasks.set_task_engine(&project, params.engine)?;
                    params.cwd
                } else {
                    let params: proto::ProjectParams =
                        serde_json::from_value(request.params).map_err(|_| invalid())?;
                    params.cwd
                };
                let project = crate::tasks::repository(&cwd, stopping)?;
                let (engine, configured) = tasks.task_engine(&project)?;
                Ok(serde_json::to_value(proto::ProjectTaskEngine { project, engine, configured })?)
            },
            proto::TASK_BOARD => {
                let params: proto::ProjectParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let project = crate::tasks::repository(&params.cwd, stopping)?;
                let ProjectCapacity { local, occupied } =
                    project_capacity(&project, tasks, sessions, stopping);
                let capacity = tasks.task_capacity(&project)?;
                let available = capacity.saturating_sub(occupied);
                let mut rows = Vec::new();
                for task in tasks.list() {
                    if task.status != proto::TaskStatus::Open {
                        continue;
                    }
                    let explicit = task.project.as_ref() == Some(&project);
                    let legacy = task.project.is_none()
                        && task.sessions.iter().any(|id| local.contains_key(id));
                    if !explicit && !legacy {
                        continue;
                    }
                    let dependencies =
                        dependency_snapshot(&task, tasks, reviews, landings, sessions, stopping)?;
                    let state = if legacy {
                        proto::BoardState::Manual
                    } else if task.attention.is_some()
                        || task.queue.as_ref().is_some_and(|queue| {
                            queue.state == proto::TaskQueueState::NeedsAttention
                        })
                        || task.sessions.iter().any(|id| {
                            matches!(local.get(id), None | Some(proto::SessionState::Interrupted))
                        })
                    {
                        proto::BoardState::NeedsAttention
                    } else if task
                        .landing
                        .as_ref()
                        .is_some_and(|landing| landing.state == proto::TaskLandingState::Landing)
                    {
                        proto::BoardState::Landing
                    } else if !task.drive.as_ref().is_some_and(|drive| drive.state.is_active())
                        && crate::task_landing::approval(reviews.list(Some(&task.id), None).first())
                            .is_ok()
                    {
                        proto::BoardState::AwaitingAcceptance
                    } else if task
                        .sessions
                        .iter()
                        .any(|id| local.get(id).is_some_and(|s| s.is_active()))
                    {
                        proto::BoardState::ActiveSession
                    } else if dependencies
                        .iter()
                        .any(|d| !matches!(d.state, proto::DependencyState::Included { .. }))
                    {
                        proto::BoardState::WaitingDependency
                    } else if available == 0 {
                        proto::BoardState::WaitingCapacity
                    } else {
                        proto::BoardState::Eligible
                    };
                    rows.push(proto::TaskBoardRow {
                        task_id: task.id,
                        title: task.title,
                        state,
                        attention: task.attention,
                        dependencies,
                        landing: task.landing,
                    });
                }
                let (engine, _) = tasks.task_engine(&project)?;
                Ok(serde_json::to_value(proto::TaskBoard {
                    engine,
                    project,
                    capacity,
                    occupied,
                    available,
                    tasks: rows,
                })?)
            },
            proto::TASK_LIST => Ok(serde_json::to_value(tasks.list())?),
            proto::TASK_CONTINUE => {
                let params: proto::TaskContinueParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.continue_(&params.task_id, params.rounds)?)?)
            },
            proto::TASK_CLOSE => {
                let params: proto::TaskCloseParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                let _launch_guard = runtime.lock_task_launch(&params.task_id)?;
                Ok(serde_json::to_value(tasks.close(&params.task_id, params.status)?)?)
            },
            proto::TASK_FOR_SESSION => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(tasks.for_session(&params.session_id))?)
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Ok(Response::success(request.id, value)?),
        Err(error) => Ok(rpc_failure(request.id, error)),
    }
}

fn dispatch_check(request: Request, checks: &crate::checks::Checks) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid verification parameters".into(),
        };
        match request.method.as_str() {
            proto::PROJECT_GET => {
                let params: proto::ProjectParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(checks.project_info(&params.cwd)?)?)
            },
            proto::PROJECT_TRUST => {
                let params: proto::ProjectTrustParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(checks.trust_project(&params.cwd, &params.config_hash)?)?)
            },
            proto::CHECK_PROJECT => Ok(serde_json::to_value(
                checks.project(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::CHECK_LATEST => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(checks.latest(&params.session_id)?)?)
            },
            proto::CHECK_BIND => Ok(serde_json::to_value(
                checks.bind(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::CHECK_RUN => Ok(serde_json::to_value(
                checks.run(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::CHECK_GET | proto::CHECK_CANCEL | proto::CHECK_CLEANUP => {
                let params: proto::CheckParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(if request.method == proto::CHECK_GET {
                    checks.get(&params.check_id)?
                } else if request.method == proto::CHECK_CLEANUP {
                    checks.cleanup(&params.check_id)?
                } else {
                    checks.cancel(&params.check_id)?
                })?)
            },
            proto::CHECK_LOG => {
                let params: proto::CheckLogParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(checks.log(&params.check_id, params.offset)?)?)
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Ok(Response::success(request.id, value)?),
        Err(error) => Ok(rpc_failure(request.id, error)),
    }
}

fn dispatch_checkpoint(
    request: Request,
    checkpoints: &crate::checkpoints::Checkpoints,
) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid checkpoint parameters".into(),
        };
        match request.method.as_str() {
            proto::CHECKPOINT_CREATE => Ok(serde_json::to_value(
                checkpoints
                    .create(serde_json::from_value(request.params).map_err(|_| invalid())?)?,
            )?),
            proto::CHECKPOINT_GET => {
                let params: proto::CheckpointParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(checkpoints.get(&params.checkpoint_id)?)?)
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Ok(Response::success(request.id, value)?),
        Err(error) => Ok(rpc_failure(request.id, error)),
    }
}

fn dispatch_session(request: Request, runtime: &Runtime, sessions: &Sessions) -> Result<Response> {
    let result = (|| -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(crate::Error::Rpc {
                code: proto::INVALID_REQUEST,
                message: "JSON-RPC 2.0 required".into(),
            });
        }
        let invalid = || crate::Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "invalid session parameters".into(),
        };
        match request.method.as_str() {
            proto::SESSION_RESUME => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.resume(runtime, &params.session_id)?)?)
            },
            proto::EVENTS_HISTORY => {
                let params: proto::HistoryParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.history(&params.session_id, params.after_seq)?)?)
            },
            proto::BOARD_SNAPSHOT => {
                if !request.params.is_null()
                    && !request.params.as_object().is_some_and(|params| params.is_empty())
                {
                    return Err(invalid());
                }
                Ok(serde_json::to_value(sessions.board())?)
            },
            proto::SESSION_SEND => {
                let params: proto::SendParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.control(
                    &params.session_id,
                    params.command_id,
                    proto::ControlOperation::Send { text: params.text },
                )?)?)
            },
            proto::SESSION_CANCEL => {
                let params: proto::CancelParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.control(
                    &params.session_id,
                    params.command_id,
                    proto::ControlOperation::Cancel { turn_id: params.turn_id },
                )?)?)
            },
            proto::SESSION_COMMAND => {
                let params: proto::CommandParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.command(&params.session_id, &params.command_id)?)?)
            },
            proto::APPROVAL_LIST => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.snapshot(&params.session_id)?)?)
            },
            proto::APPROVAL_RESPOND => {
                let params: proto::RespondParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.control(
                    &params.session_id,
                    params.command_id,
                    proto::ControlOperation::Respond {
                        approval_id: params.approval_id,
                        decision: params.decision,
                    },
                )?)?)
            },
            proto::SESSION_START => {
                let params = serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.start(runtime, params)?)?)
            },
            proto::CLAUDE_START => {
                let params = serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.start_claude(runtime, params)?)?)
            },
            proto::SESSION_LIST => {
                if !request.params.is_null()
                    && !request.params.as_object().is_some_and(|params| params.is_empty())
                {
                    return Err(invalid());
                }
                Ok(serde_json::to_value(sessions.list())?)
            },
            proto::SESSION_END_TOOLS => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                Ok(serde_json::to_value(sessions.end_tools(&params.session_id)?)?)
            },
            method @ (proto::SESSION_GET
            | proto::SESSION_ATTACH_INFO
            | proto::SESSION_STOP
            | proto::SESSION_STOP_IDLE) => {
                let params: proto::SessionParams =
                    serde_json::from_value(request.params).map_err(|_| invalid())?;
                match method {
                    proto::SESSION_GET => {
                        Ok(serde_json::to_value(sessions.get(&params.session_id)?)?)
                    },
                    proto::SESSION_ATTACH_INFO => {
                        Ok(serde_json::to_value(sessions.attach_info(&params.session_id)?)?)
                    },
                    proto::SESSION_STOP_IDLE => {
                        Ok(serde_json::to_value(sessions.stop_idle(&params.session_id)?)?)
                    },
                    _ => Ok(serde_json::to_value(sessions.stop(&params.session_id)?)?),
                }
            },
            _ => Err(crate::Error::Rpc {
                code: proto::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            }),
        }
    })();
    match result {
        Ok(value) => Response::success(request.id, value).map_err(Into::into),
        Err(crate::Error::Rpc { code, message }) => {
            Ok(Response::failure(Some(request.id), code, &message))
        },
        Err(error) => {
            Ok(Response::failure(Some(request.id), proto::ENGINE_ERROR, &error.to_string()))
        },
    }
}

fn rpc_failure(id: proto::RequestId, error: crate::Error) -> Response {
    match error {
        crate::Error::Rpc { code, message } => Response::failure(Some(id), code, &message),
        error => Response::failure(Some(id), proto::ENGINE_ERROR, &error.to_string()),
    }
}

/// Push the whole board after each change (coalesced over 50 ms), with heartbeats while idle.
fn stream_board(
    stream: &mut UnixStream,
    sessions: &Sessions,
    _slot: crate::sessions::BoardWatch,
    mut seen: u64,
    stopping: &AtomicBool,
) -> Result<()> {
    while !stopping.load(Ordering::Acquire) {
        let version = crate::events::BOARD.wait(seen, Duration::from_millis(500));
        let (method, params) = if version == seen {
            (proto::EVENTS_HEARTBEAT, Value::Null)
        } else {
            thread::sleep(Duration::from_millis(50));
            seen = crate::events::BOARD.version();
            (proto::BOARD_CHANGED, serde_json::to_value(sessions.board())?)
        };
        write_frame(stream, &proto::Notification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        })?;
    }
    Ok(())
}

/// Stored events after the client's cursor first, then live ones: one gapless sequence.
fn stream_events(
    stream: &mut UnixStream,
    subscription: Subscription,
    replay: Vec<proto::SessionEvent>,
    stopping: &AtomicBool,
) -> Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    for event in replay {
        write_frame(stream, &proto::Notification {
            jsonrpc: "2.0".into(),
            method: proto::EVENTS_EVENT.into(),
            params: serde_json::to_value(event)?,
        })?;
    }
    while !stopping.load(Ordering::Acquire) {
        if subscription.overflow.load(Ordering::Acquire) {
            write_frame(stream, &proto::Notification {
                jsonrpc: "2.0".into(),
                method: proto::EVENTS_RESYNC.into(),
                params: Value::Null,
            })?;
            break;
        }
        let (method, params) = match subscription.receiver.recv_timeout(Duration::from_millis(500))
        {
            Ok(event) => (proto::EVENTS_EVENT, serde_json::to_value(event)?),
            Err(RecvTimeoutError::Timeout) => (proto::EVENTS_HEARTBEAT, Value::Null),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if subscription.overflow.load(Ordering::Acquire) {
            continue;
        }
        write_frame(stream, &proto::Notification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_pre_dispatch_gates_are_rechecked_by_the_queue() {
        for message in [
            "project task capacity is occupied (limit 1); stop or reconcile",
            "another task launch is in progress for this project",
            "another launch operation is in progress for this task",
            "prerequisite A is not satisfied in the launch checkout: \
             {\"kind\":\"awaiting_landing\"}",
        ] {
            assert!(queue_waits_for(&crate::Error::Rpc {
                code: proto::CONFLICT,
                message: message.into()
            }));
        }
        for message in [
            "launch requires reconciliation for session old",
            "engine refused the prompt",
            "prerequisite A is not satisfied in the launch checkout: {\"kind\":\"abandoned\"}",
            "prerequisite A is not satisfied in the launch checkout: \
             {\"kind\":\"multiple_landing_targets\"}",
        ] {
            assert!(!queue_waits_for(&crate::Error::Rpc {
                code: proto::CONFLICT,
                message: message.into()
            }));
        }
        assert!(!queue_waits_for(&crate::Error::Protocol("persistent state failed")));
    }

    fn code(response: Response) -> i32 {
        let Response::Failure(failure) = response else { panic!("expected failure") };
        failure.error.code
    }

    #[test]
    fn initialization_is_required_and_version_mismatch_does_not_initialize() {
        let mut initialized = false;
        let request = Request::new(1, proto::STATUS, Value::Null).unwrap();
        assert_eq!(
            code(dispatch(request, &mut initialized, Instant::now(), 0).unwrap().0),
            proto::NOT_INITIALIZED
        );
        let request = Request::new(2, proto::INITIALIZE, InitializeParams {
            protocol_version: proto::PROTOCOL_VERSION + 1,
            client_name: "test".into(),
        })
        .unwrap();
        assert_eq!(
            code(dispatch(request, &mut initialized, Instant::now(), 0).unwrap().0),
            proto::VERSION_MISMATCH
        );
        assert!(!initialized);
    }

    #[test]
    fn unknown_methods_and_invalid_parameters_never_trigger_shutdown() {
        let mut initialized = true;
        let request = Request::new(1, "missing", Value::Null).unwrap();
        let (response, stop) = dispatch(request, &mut initialized, Instant::now(), 0).unwrap();
        assert_eq!(code(response), proto::METHOD_NOT_FOUND);
        assert!(!stop);
        let request =
            Request::new(2, proto::SHUTDOWN, serde_json::json!({"unexpected":true})).unwrap();
        let (response, stop) = dispatch(request, &mut initialized, Instant::now(), 0).unwrap();
        assert_eq!(code(response), proto::INVALID_PARAMS);
        assert!(!stop);
    }
}
