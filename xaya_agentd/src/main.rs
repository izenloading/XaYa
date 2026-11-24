#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use std::{env, thread};

    use clap::{Args, Parser, Subcommand};
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::flag;
    use xaya_agent_proto::{self as proto, AttachInfo, Session, SessionParams, StartSessionParams};
    use xaya_agentd::client::Client;
    use xaya_agentd::runtime::Runtime;
    use xaya_agentd::{Error, Result};

    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command};

    #[derive(Parser)]
    #[command(name = "xaya-agentd", version, about = "XaYa local agent service")]
    struct Options {
        /// Private service directory (parent must exist).
        #[arg(long, env = "XAYA_AGENT_RUNTIME_DIR", global = true)]
        runtime_dir: Option<PathBuf>,
        /// Persistent state directory; custom runtime directories default to their own state/.
        #[arg(long, env = "XAYA_AGENT_STATE_DIR", global = true)]
        state_dir: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<Action>,
    }

    #[derive(Subcommand)]
    enum Action {
        /// Internal: connect this terminal to a PTY host (Ctrl+] or Ctrl+Z detaches).
        #[command(hide = true)]
        PtyAttach {
            #[arg(long)]
            dir: PathBuf,
        },
        /// Internal: detached PTY host for one managed program (see `pty_host`).
        #[command(hide = true)]
        PtyHost {
            #[arg(long)]
            dir: PathBuf,
            #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
            rows: u16,
            #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
            cols: u16,
            #[arg(long, requires = "relay_to")]
            relay_fd: Option<i32>,
            #[arg(long, requires = "relay_fd")]
            relay_to: Option<PathBuf>,
            #[arg(last = true, required = true)]
            argv: Vec<String>,
        },
        /// Start a managed engine in an existing directory and attach its native TUI.
        Run(RunOptions),
        /// Reattach to a session; closing the TUI leaves the backend running.
        Attach { session_id: String },
        /// Restore a terminal's Codex thread without starting a second managed writer.
        Restore { native_thread_id: String },
        /// Capture a managed workspace without changing its branch or staging area.
        Checkpoint {
            #[command(subcommand)]
            command: CheckpointAction,
        },
        /// Run explicit verification commands bound to an existing checkpoint.
        Check {
            #[command(subcommand)]
            command: CheckAction,
        },
        /// Tasks: a goal and testable acceptance criteria for sessions, used by reviews.
        Task {
            #[command(subcommand)]
            command: TaskAction,
        },
        /// Review a checkpoint with a headless engine in a private, read-only checkout.
        Review {
            #[command(subcommand)]
            command: ReviewAction,
        },
        /// Inspect and explicitly trust repository verification configuration.
        Project {
            #[command(subcommand)]
            command: ProjectAction,
        },
        /// Build and recheck a private squash landing preview.
        Land {
            #[command(subcommand)]
            command: LandAction,
        },
        /// Inspect or stop managed sessions.
        Session {
            #[command(subcommand)]
            command: SessionAction,
        },
        /// Run the service in the foreground (normally started on demand).
        Serve {
            #[arg(long, default_value_t = 600)]
            idle_timeout_secs: u64,
        },
        /// Start the service if necessary and report its status.
        Start {
            #[arg(long)]
            json: bool,
        },
        /// Query service status, starting the service unless --no-start is set.
        Status {
            #[arg(long)]
            json: bool,
            #[arg(long)]
            no_start: bool,
        },
        /// Stop the service; a stopped service stays stopped.
        Stop {
            #[arg(long)]
            json: bool,
        },
    }

    #[derive(Args)]
    struct RunOptions {
        #[arg(long, default_value = "codex", value_parser = ["codex", "claude"])]
        engine: String,
        #[arg(short = 'C', long, default_value = ".")]
        cwd: PathBuf,
        /// Codex executable, resolved using this caller's PATH.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Claude executable, resolved using this caller's PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex sandbox; defaults to read-only. Not accepted for Claude manual permissions.
        #[arg(long, value_parser = ["read-only", "workspace-write"])]
        sandbox: Option<String>,
        /// Start without attaching. Print the session ID.
        #[arg(long)]
        detach: bool,
        /// Start without attaching and print the session record as JSON.
        #[arg(long)]
        json: bool,
        /// Optional first prompt, submitted by the native TUI.
        prompt: Option<String>,
    }

    #[derive(Args)]
    struct TaskSpec {
        /// Include this task in the read-only board for this repository.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Existing prerequisite task ID; repeat for multiple prerequisites.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        goal: String,
        /// A testable acceptance criterion; repeat for each (1-20).
        #[arg(long = "criterion", required = true)]
        criteria: Vec<String>,
        #[arg(long)]
        constraint: Option<String>,
        /// Review rounds that may be sent back before a person decides (0-5, default 2).
        #[arg(long)]
        fix_budget: Option<u32>,
        /// Require review from both Codex and Claude.
        #[arg(long)]
        dual_review: bool,
    }

    #[derive(Subcommand)]
    enum TaskAction {
        /// Create a task, optionally for existing sessions.
        New {
            #[command(flatten)]
            spec: TaskSpec,
            /// A session the task is for; repeat for several.
            #[arg(long = "session")]
            sessions: Vec<String>,
        },
        /// Create a task for one existing session.
        Promote {
            session_id: String,
            #[command(flatten)]
            spec: TaskSpec,
        },
        /// The task, its reviews and each session's latest verification.
        Show {
            task_id: String,
        },
        /// Show this repository's open tasks and available session capacity.
        Board {
            #[arg(default_value = ".")]
            directory: PathBuf,
        },
        /// Read or set this repository's task launch capacity (shared by all worktrees).
        Capacity {
            #[arg(default_value = ".")]
            directory: PathBuf,
            /// Maximum simultaneous task sessions (1-16); lowering it leaves sessions running.
            #[arg(long, value_parser = clap::value_parser!(u16).range(1..=16))]
            limit: Option<u16>,
        },
        /// Read or set the engine this repository's delegated tasks are implemented with unless
        /// one is named (shared by all worktrees; Claude without a setting).
        Engine {
            #[arg(default_value = ".")]
            directory: PathBuf,
            /// Make this engine the default.
            #[arg(long, value_enum, conflicts_with = "reset")]
            set: Option<ReviewEngineArg>,
            /// Remove the setting: Claude implements again.
            #[arg(long)]
            reset: bool,
        },
        /// Read the open task's prerequisites for a session.
        Dependencies {
            #[arg(long)]
            session: String,
        },
        /// Claim a task and launch an agent in a project checkout, or in its own workspace.
        Start {
            task_id: String,
            #[arg(long)]
            request_id: String,
            #[arg(long, default_value = ".")]
            cwd: PathBuf,
            /// Let the service create a detached worktree for the task from this local branch's
            /// tip, beside the project at `<parent>/<repository>.xaya/<task ID>`.
            #[arg(long, value_name = "BASE_BRANCH", conflicts_with = "cwd")]
            workspace: Option<String>,
            #[arg(long, value_enum, default_value = "claude")]
            engine: ReviewEngineArg,
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            /// Codex sandbox mode. Defaults to read-only.
            #[arg(long, value_parser = ["read-only", "workspace-write"])]
            sandbox: Option<String>,
        },
        /// Queue the task to start automatically when capacity and prerequisites permit. Codex
        /// then takes the task as its first message without a native TUI; attach any time.
        Enqueue {
            task_id: String,
            #[arg(long)]
            request_id: String,
            /// Existing task checkout; or use --workspace for a separate one per task.
            #[arg(long, default_value = ".")]
            cwd: PathBuf,
            /// Let the service create a detached worktree for the task from this local branch's
            /// tip, beside the project at `<parent>/<repository>.xaya/<task ID>`.
            #[arg(long, value_name = "BASE_BRANCH", conflicts_with = "cwd")]
            workspace: Option<String>,
            #[arg(long, value_enum, default_value = "claude")]
            engine: ReviewEngineArg,
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            /// Codex sandbox mode. Defaults to read-only.
            #[arg(long, value_parser = ["read-only", "workspace-write"])]
            sandbox: Option<String>,
        },
        /// Hand over one request in one step: the service creates the task, launches an agent in
        /// a workspace of its own from the branch checked out here, and reviews and sends fixes
        /// back until approved or a person has to decide. A repeated request ID returns the task.
        Delegate {
            /// What you want done; its first line becomes the title.
            goal: String,
            #[arg(long)]
            request_id: String,
            /// A checkout of the project.
            #[arg(long, default_value = ".")]
            cwd: PathBuf,
            /// The workspace's base; by default the branch checked out in --cwd.
            #[arg(long)]
            base_branch: Option<String>,
            /// The implementing engine; by default the project's (`task engine`), else Claude.
            #[arg(long, value_enum)]
            engine: Option<ReviewEngineArg>,
            /// Defaults to the engine other than the implementing one.
            #[arg(long, value_enum)]
            reviewer: Option<ReviewEngineArg>,
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            /// Time limit for a review and, separately, each fix turn.
            #[arg(long, default_value_t = proto::REVIEW_DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..=3600))]
            timeout_secs: u64,
        },
        /// Land the change the task's latest review approved, in the background: preview it
        /// onto the target branch, verify it there, update the branch and accept the task.
        Land {
            task_id: String,
            #[arg(long)]
            request_id: String,
            /// Local branch to land on; by default the base branch of the task's workspace.
            #[arg(long)]
            target: Option<String>,
        },
        /// Ask for changes to a driven task's reviewed change: they go to its implementing session,
        /// and the service verifies and reviews again.
        Revise {
            task_id: String,
            #[arg(long)]
            request_id: String,
            /// What should change.
            feedback: String,
        },
        /// What accepting the task takes: its reviewed change and diff, verification, review,
        /// landing, and why it cannot land now, if it cannot.
        Acceptance {
            task_id: String,
        },
        /// Cancel a queued task before its session is claimed.
        Dequeue {
            task_id: String,
            #[arg(long)]
            request_id: String,
        },
        /// Let the service checkpoint, verify, review and send fixes back in the background until
        /// the task is approved or a person has to decide. Needs no terminal to stay open.
        Drive {
            task_id: String,
            #[arg(long)]
            request_id: String,
            /// The implementing session; by default the task's only session, once it has one.
            #[arg(long)]
            session: Option<String>,
            /// Defaults to the engine other than the implementing one.
            #[arg(long, value_enum)]
            engine: Option<ReviewEngineArg>,
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
            /// Time limit for a review and, separately, each fix turn.
            #[arg(long, default_value_t = proto::REVIEW_DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..=3600))]
            timeout_secs: u64,
        },
        /// Stop driving a task in the background; steps already taken stand.
        Undrive {
            task_id: String,
            #[arg(long)]
            request_id: String,
        },
        List,
        /// After deciding: clear the task's need for attention and optionally grant more fix
        /// rounds.
        Continue {
            task_id: String,
            #[arg(long, default_value_t = 0)]
            rounds: u32,
        },
        /// Checkpoint, verify and review until approved or a person needs to decide.
        Cycle {
            task_id: String,
            /// Required when the task contains more than one session.
            #[arg(long)]
            session: Option<String>,
            /// Defaults to the other engine.
            #[arg(long, value_enum)]
            engine: Option<ReviewEngineArg>,
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
            /// Time limit for a review and, separately, each implementing turn.
            #[arg(long, default_value_t = proto::REVIEW_DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..=3600))]
            timeout_secs: u64,
        },
        Close {
            task_id: String,
            #[arg(long, required_unless_present = "abandoned", conflicts_with = "abandoned")]
            accepted: bool,
            #[arg(long)]
            abandoned: bool,
        },
    }

    #[derive(Subcommand)]
    enum CheckpointAction {
        Create {
            session_id: String,
            #[arg(long)]
            command_id: Option<String>,
            /// Return the durable Creating receipt without waiting for Git.
            #[arg(long)]
            no_wait: bool,
            #[arg(long)]
            json: bool,
        },
        Show {
            checkpoint_id: String,
        },
    }

    #[derive(Subcommand)]
    enum LandAction {
        /// Re-run the original check plan in a private checkout of the integrated candidate.
        Verify {
            preview_id: String,
            #[arg(long)]
            no_wait: bool,
        },
        /// Advance a local target branch after integrated verification.
        Run {
            preview_id: String,
        },
        /// Inspect an interrupted landing without replay or rollback.
        Reconcile {
            preview_id: String,
        },
        /// Finish an interrupted landing when the target provably holds only the old version,
        /// the candidate, or a mix of both; never overwrites other content.
        Resume {
            preview_id: String,
        },
        Preview {
            checkpoint_id: String,
            #[arg(long)]
            check_id: String,
            #[arg(long)]
            target: String,
            #[arg(long)]
            command_id: Option<String>,
            #[arg(long)]
            no_wait: bool,
        },
        Show {
            preview_id: String,
        },
        Recheck {
            preview_id: String,
        },
    }

    #[derive(Subcommand)]
    enum ProjectAction {
        Show {
            #[arg(short = 'C', long, default_value = ".")]
            cwd: PathBuf,
        },
        /// Trust only the exact configuration hash previously reviewed with project show.
        Trust {
            #[arg(short = 'C', long, default_value = ".")]
            cwd: PathBuf,
            #[arg(long)]
            hash: String,
        },
    }

    #[derive(Clone, Copy, clap::ValueEnum)]
    enum ReviewEngineArg {
        Codex,
        Claude,
    }

    #[derive(Subcommand)]
    enum ReviewAction {
        /// Start a review and, unless --no-wait, wait for its verdict. Exits non-zero unless the
        /// verdict is approved.
        Run {
            checkpoint_id: String,
            /// By default the engine the implementing session did not use.
            #[arg(long, value_enum)]
            engine: Option<ReviewEngineArg>,
            /// A verification of the same checkpoint; nothing is approved without a passing one.
            #[arg(long = "check")]
            check_id: Option<String>,
            /// Review the change from this ancestor; by default the checkpoint's own base.
            #[arg(long)]
            base: Option<String>,
            #[arg(long)]
            command_id: Option<String>,
            #[arg(long, default_value_t = proto::REVIEW_DEFAULT_TIMEOUT_SECS,
                  value_parser = clap::value_parser!(u64).range(1..=3600))]
            timeout_secs: u64,
            #[arg(long)]
            no_wait: bool,
            /// Review against this task's criteria; by default the open task of the
            /// checkpoint's session, if any.
            #[arg(long, conflicts_with = "no_task")]
            task: Option<String>,
            /// Review without a task even if the session belongs to one.
            #[arg(long)]
            no_task: bool,
            /// Codex executable, resolved using this caller's PATH.
            #[arg(long, default_value = "codex")]
            codex: PathBuf,
            /// Claude executable, resolved using this caller's PATH.
            #[arg(long, default_value = "claude")]
            claude: PathBuf,
        },
        /// The review and its verdict; also re-checks whether the checkout still matches.
        Show {
            review_id: String,
        },
        /// Dispute one finding of a task review (numbered from 1), with a reason: it is left
        /// out of fixes and of the loop's breakers; verdicts do not change.
        Dispute {
            review_id: String,
            finding: usize,
            #[arg(long)]
            reason: String,
        },
        /// Reviews as JSON, newest first.
        List {
            #[arg(long)]
            task: Option<String>,
            #[arg(long)]
            session: Option<String>,
        },
        /// Send a needs-fix review's findings back to the implementing session, once; a
        /// stopped session is resumed first. Waits for the send to be confirmed unless
        /// --no-wait.
        Send {
            review_id: String,
            #[arg(long)]
            command_id: Option<String>,
            #[arg(long)]
            no_wait: bool,
        },
        Cancel {
            review_id: String,
        },
    }

    #[derive(Subcommand)]
    enum CheckAction {
        /// Execute the trusted project steps as one verification run.
        Project {
            checkpoint_id: String,
            #[arg(long)]
            command_id: Option<String>,
            #[arg(long)]
            config_hash: Option<String>,
            #[arg(long)]
            no_wait: bool,
        },
        /// Latest recorded verification for one managed session; refreshes freshness.
        Latest {
            session_id: String,
        },
        Run {
            checkpoint_id: String,
            #[arg(long)]
            command_id: Option<String>,
            #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
            timeout_secs: u64,
            #[arg(long)]
            no_wait: bool,
            /// Executable and arguments, passed directly without a shell.
            #[arg(last = true, required = true)]
            argv: Vec<String>,
        },
        Show {
            check_id: String,
        },
        /// Prove in private checkouts that a passing check's gate reads the tree it names.
        Bind {
            check_id: String,
            #[arg(long)]
            command_id: Option<String>,
            /// Tracked file to corrupt; by default a non-test source file the change touched.
            #[arg(long)]
            path: Option<String>,
            #[arg(long)]
            no_wait: bool,
        },
        Cancel {
            check_id: String,
        },
        /// Remove only a recorded integration checkout whose process group has ended.
        Cleanup {
            check_id: String,
        },
        Log {
            check_id: String,
            #[arg(long, default_value_t = 0)]
            offset: usize,
        },
    }

    #[derive(Subcommand)]
    enum SessionAction {
        Resume {
            session_id: String,
        },
        /// Compact snapshot used by the terminal's managed-session panel.
        Board {
            /// Keep printing the board (one JSON line each time any session changes).
            #[arg(long)]
            watch: bool,
        },
        /// Send one turn; query the returned command_id for the engine acknowledgement.
        Send {
            session_id: String,
            text: String,
            #[arg(long)]
            command_id: Option<String>,
        },
        /// Interrupt the current turn. Does not claim that every tool process has exited.
        Cancel {
            session_id: String,
            #[arg(long)]
            turn_id: Option<String>,
            #[arg(long)]
            command_id: Option<String>,
        },
        /// Query a command after an acknowledgement or an uncertain transport failure.
        Command {
            session_id: String,
            command_id: String,
        },
        /// Print a snapshot followed by ordered JSON events; never replay commands on reconnect.
        Watch {
            session_id: String,
            /// Continue after the last event seen (`EPOCH:SEQ`): stored events arrive first.
            #[arg(long, value_parser = cursor)]
            after: Option<proto::EventCursor>,
        },
        /// End the session's tool processes (and their descendants), keeping the engine running.
        /// Refused while a turn runs: cancel it first.
        EndTools {
            session_id: String,
        },
        /// Print the session's stored events as JSON lines, oldest first.
        History {
            session_id: String,
            /// Only events after this sequence of the current stream.
            #[arg(long, default_value_t = 0)]
            after: u64,
        },
        /// Show pending command/file approvals and the current session snapshot.
        Approvals {
            session_id: String,
        },
        /// Answer an approval once using one of its available decisions.
        Respond {
            session_id: String,
            approval_id: String,
            #[arg(value_parser = ["accept", "decline", "cancel"])]
            decision: String,
            #[arg(long)]
            command_id: Option<String>,
        },
        List {
            #[arg(long)]
            json: bool,
        },
        Show {
            session_id: String,
        },
        AttachInfo {
            session_id: String,
        },
        Stop {
            session_id: String,
        },
    }

    fn cursor(value: &str) -> std::result::Result<proto::EventCursor, String> {
        let (epoch, seq) = value.split_once(':').ok_or("expected EPOCH:SEQ")?;
        let seq = seq.parse().map_err(|_| "SEQ must be a number")?;
        if epoch.is_empty() || epoch.len() > 64 || !epoch.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("EPOCH must be the hexadecimal epoch of a snapshot".into());
        }
        Ok(proto::EventCursor { epoch: epoch.into(), seq })
    }

    /// The caller's engine configuration directory, only if the caller set `variable`. Claude
    /// on macOS keys its Keychain login by whether CLAUDE_CONFIG_DIR is set at all, so a default
    /// is never filled in: the engine finds its own.
    fn config_dir(variable: &str) -> Result<Option<String>> {
        let Some(path) = env::var_os(variable) else { return Ok(None) };
        Ok(Some(
            PathBuf::from(path)
                .canonicalize()?
                .into_os_string()
                .into_string()
                .map_err(|_| Error::Protocol("configuration paths must be UTF-8"))?,
        ))
    }

    fn task(runtime: &Runtime, state_dir: Option<&Path>, command: TaskAction) -> Result<()> {
        let create = |spec: TaskSpec, sessions: Vec<String>| -> Result<proto::TaskCreateParams> {
            Ok(proto::TaskCreateParams {
                project: spec
                    .project
                    .map(std::fs::canonicalize)
                    .transpose()?
                    .map(|p| p.display().to_string()),
                depends_on: spec.depends_on,
                title: spec.title,
                goal: spec.goal,
                constraints: spec.constraint,
                criteria: spec.criteria,
                sessions,
                fix_budget: spec.fix_budget,
                dual_review: spec.dual_review,
            })
        };
        let value = match command {
            TaskAction::New { spec, sessions } => {
                let mut client =
                    Client::ensure_running_with_state(runtime, &env::current_exe()?, state_dir)?;
                client.request::<serde_json::Value>(proto::TASK_CREATE, create(spec, sessions)?)?
            },
            TaskAction::Promote { session_id, spec } => {
                let mut client = Client::connect(runtime)?;
                client.request(proto::TASK_CREATE, create(spec, vec![session_id])?)?
            },
            TaskAction::Cycle { task_id, session, engine, codex, claude, timeout_secs } => {
                let result = xaya_agentd::cycle::run(
                    runtime,
                    &task_id,
                    session,
                    timeout_secs,
                    |session, dual| review_engines(engine, &session.engine, &codex, &claude, dual),
                )?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                return if result.outcome == xaya_agentd::cycle::Outcome::Approved {
                    Ok(())
                } else {
                    Err(Error::Protocol("the task cycle stopped before approval"))
                };
            },
            TaskAction::Start {
                task_id,
                request_id,
                cwd,
                workspace,
                engine,
                claude,
                codex,
                sandbox,
            } => {
                let mut client = Client::connect(runtime)?;
                let task: proto::Task = client
                    .request(proto::TASK_GET, proto::TaskParams { task_id: task_id.clone() })?;
                let (claude, codex) =
                    launch_engines(&task, &cwd, engine, &claude, &codex, sandbox.as_deref())?;
                client.request(proto::TASK_START, proto::TaskStartParams {
                    task_id,
                    request_id,
                    claude,
                    codex,
                    workspace: workspace.map(|base_branch| proto::TaskWorkspace { base_branch }),
                })?
            },
            TaskAction::Enqueue {
                task_id,
                request_id,
                cwd,
                workspace,
                engine,
                claude,
                codex,
                sandbox,
            } => {
                let mut client = Client::connect(runtime)?;
                let task: proto::Task = client
                    .request(proto::TASK_GET, proto::TaskParams { task_id: task_id.clone() })?;
                let (claude, codex) =
                    launch_engines(&task, &cwd, engine, &claude, &codex, sandbox.as_deref())?;
                client.request(proto::TASK_ENQUEUE, proto::TaskEnqueueParams {
                    task_id,
                    request_id,
                    claude,
                    codex,
                    workspace: workspace.map(|base_branch| proto::TaskWorkspace { base_branch }),
                })?
            },
            TaskAction::Drive {
                task_id,
                request_id,
                session,
                engine,
                codex,
                claude,
                timeout_secs,
            } => {
                let mut client = Client::connect(runtime)?;
                let task: proto::Task = client
                    .request(proto::TASK_GET, proto::TaskParams { task_id: task_id.clone() })?;
                // The default reviewer is the other engine, including a queued task's engine.
                let implementer = match (&session, task.sessions.as_slice(), &task.queue) {
                    (Some(id), ..) | (None, [id], _) => {
                        client
                            .request::<proto::Session>(proto::SESSION_GET, proto::SessionParams {
                                session_id: id.clone(),
                            })?
                            .engine
                    },
                    (None, [], Some(queue)) => {
                        if queue.codex.is_some() { "codex" } else { "claude" }.into()
                    },
                    _ if engine.is_some() => String::new(),
                    _ => {
                        return Err(Error::Protocol(
                            "choose the reviewer with --engine: the task has no single session yet",
                        ));
                    },
                };
                let (reviewer, second_reviewer) =
                    review_engines(engine, &implementer, &codex, &claude, task.dual_review)?;
                client.request(proto::TASK_DRIVE, proto::TaskDriveParams {
                    task_id,
                    request_id,
                    session_id: session,
                    reviewer,
                    second_reviewer,
                    timeout_secs,
                })?
            },
            TaskAction::Delegate {
                goal,
                request_id,
                cwd,
                base_branch,
                engine,
                reviewer,
                claude,
                codex,
                timeout_secs,
            } => {
                let mut client =
                    Client::ensure_running_with_state(runtime, &env::current_exe()?, state_dir)?;
                let cwd = cwd
                    .canonicalize()?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| Error::Protocol("project checkout path must be UTF-8"))?;
                let engine = match engine {
                    Some(engine) => engine,
                    None => {
                        let project: proto::ProjectTaskEngine = serde_json::from_value(
                            client.request(proto::TASK_ENGINE_GET, proto::ProjectParams {
                                cwd: cwd.clone(),
                            })?,
                        )?;
                        match project.engine {
                            proto::ReviewEngine::Claude => ReviewEngineArg::Claude,
                            proto::ReviewEngine::Codex => ReviewEngineArg::Codex,
                        }
                    },
                };
                let mut implementer = review_engine(Some(engine), "", &codex, &claude)?;
                if implementer.engine == proto::ReviewEngine::Codex {
                    implementer.config_dir = Some(codex_home()?);
                }
                let implementing = match engine {
                    ReviewEngineArg::Codex => "codex",
                    ReviewEngineArg::Claude => "claude",
                };
                let reviewer = review_engine(reviewer, implementing, &codex, &claude)?;
                client.request(proto::TASK_DELEGATE, proto::TaskDelegateParams {
                    request_id,
                    cwd,
                    goal,
                    base_branch,
                    implementer,
                    reviewer,
                    timeout_secs,
                })?
            },
            TaskAction::Land { task_id, request_id, target } => {
                Client::connect(runtime)?.request(proto::TASK_LAND, proto::TaskLandParams {
                    task_id,
                    request_id,
                    target_branch: target,
                })?
            },
            TaskAction::Revise { task_id, request_id, feedback } => Client::connect(runtime)?
                .request(proto::TASK_REVISE, proto::TaskReviseParams {
                    task_id,
                    request_id,
                    feedback,
                })?,
            TaskAction::Acceptance { task_id } => Client::connect(runtime)?
                .request(proto::TASK_ACCEPTANCE, proto::TaskParams { task_id })?,
            TaskAction::Undrive { task_id, request_id } => Client::connect(runtime)?
                .request(proto::TASK_UNDRIVE, proto::TaskUndriveParams { task_id, request_id })?,
            TaskAction::Dequeue { task_id, request_id } => Client::connect(runtime)?
                .request(proto::TASK_DEQUEUE, proto::TaskDequeueParams { task_id, request_id })?,
            TaskAction::Board { directory } => Client::connect(runtime)?
                .request(proto::TASK_BOARD, proto::ProjectParams {
                    cwd: directory.canonicalize()?.display().to_string(),
                })?,
            TaskAction::Capacity { directory, limit } => {
                let cwd = directory
                    .canonicalize()?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| Error::Protocol("project path must be UTF-8"))?;
                let mut client = Client::connect(runtime)?;
                match limit {
                    Some(capacity) => {
                        client.request(proto::TASK_CAPACITY_SET, proto::TaskCapacityParams {
                            cwd,
                            capacity: usize::from(capacity),
                        })?
                    },
                    None => {
                        client.request(proto::TASK_CAPACITY_GET, proto::ProjectParams { cwd })?
                    },
                }
            },
            TaskAction::Engine { directory, set, reset } => {
                let cwd = directory
                    .canonicalize()?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| Error::Protocol("project path must be UTF-8"))?;
                let mut client =
                    Client::ensure_running_with_state(runtime, &env::current_exe()?, state_dir)?;
                if set.is_none() && !reset {
                    client.request(proto::TASK_ENGINE_GET, proto::ProjectParams { cwd })?
                } else {
                    let engine = set.map(|engine| match engine {
                        ReviewEngineArg::Claude => proto::ReviewEngine::Claude,
                        ReviewEngineArg::Codex => proto::ReviewEngine::Codex,
                    });
                    client
                        .request(proto::TASK_ENGINE_SET, proto::TaskEngineParams { cwd, engine })?
                }
            },
            TaskAction::Dependencies { session } => {
                let mut client = Client::connect(runtime)?;
                let current: Option<proto::Task> = client
                    .request(proto::TASK_FOR_SESSION, SessionParams { session_id: session })?;
                match current {
                    Some(task) if !task.depends_on.is_empty() => client
                        .request(proto::TASK_DEPENDENCIES, proto::TaskParams {
                            task_id: task.id,
                        })?,
                    _ => serde_json::json!([]),
                }
            },
            TaskAction::List => {
                Client::connect(runtime)?.request(proto::TASK_LIST, serde_json::json!({}))?
            },
            TaskAction::Continue { task_id, rounds } => Client::connect(runtime)?
                .request(proto::TASK_CONTINUE, proto::TaskContinueParams { task_id, rounds })?,
            TaskAction::Close { task_id, accepted, .. } => {
                let status = if accepted {
                    proto::TaskStatus::Accepted
                } else {
                    proto::TaskStatus::Abandoned
                };
                Client::connect(runtime)?
                    .request(proto::TASK_CLOSE, proto::TaskCloseParams { task_id, status })?
            },
            TaskAction::Show { task_id } => {
                let mut client = Client::connect(runtime)?;
                let task: proto::Task = client
                    .request(proto::TASK_GET, proto::TaskParams { task_id: task_id.clone() })?;
                let dependencies: Vec<proto::DependencySnapshot> = if task.depends_on.is_empty() {
                    Vec::new()
                } else {
                    client.request(proto::TASK_DEPENDENCIES, proto::TaskParams {
                        task_id: task_id.clone(),
                    })?
                };
                let reviews: serde_json::Value =
                    client.request(proto::REVIEW_LIST, proto::ReviewListParams {
                        task_id: Some(task_id),
                        session_id: None,
                    })?;
                let mut checks = serde_json::Map::new();
                for session in &task.sessions {
                    let latest: serde_json::Value = client
                        .request(proto::CHECK_LATEST, SessionParams {
                            session_id: session.clone(),
                        })?;
                    checks.insert(session.clone(), latest);
                }
                serde_json::json!({"task": task, "reviews": reviews, "latest_checks": checks, "dependencies": dependencies})
            },
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }

    /// Codex's configuration directory: CODEX_HOME, or `~/.codex`.
    fn codex_home() -> Result<String> {
        let home = match env::var_os("CODEX_HOME") {
            Some(home) => PathBuf::from(home),
            None => PathBuf::from(env::var_os("HOME").ok_or(Error::Protocol("HOME is not set"))?)
                .join(".codex"),
        };
        std::path::absolute(home)?
            .into_os_string()
            .into_string()
            .map_err(|_| Error::Protocol("Codex configuration path must be UTF-8"))
    }

    /// The launch of a task in `cwd`, with the task as the engine's first message.
    fn launch_engines(
        task: &proto::Task,
        cwd: &Path,
        engine: ReviewEngineArg,
        claude: &Path,
        codex: &Path,
        sandbox: Option<&str>,
    ) -> Result<(Option<proto::StartClaudeParams>, Option<StartSessionParams>)> {
        let cwd = cwd
            .canonicalize()?
            .into_os_string()
            .into_string()
            .map_err(|_| Error::Protocol("task checkout path must be UTF-8"))?;
        let executable = |path: &Path| {
            resolve_executable(path)?
                .into_os_string()
                .into_string()
                .map_err(|_| Error::Protocol("engine executable path must be UTF-8"))
        };
        Ok(match engine {
            ReviewEngineArg::Claude => {
                if sandbox.is_some() {
                    return Err(Error::Protocol("--sandbox applies to Codex"));
                }
                let prompt = xaya_agentd::task_prompt(task);
                (
                    Some(proto::StartClaudeParams {
                        cwd,
                        executable: executable(claude)?,
                        config_dir: config_dir("CLAUDE_CONFIG_DIR")?,
                        prompt,
                    }),
                    None,
                )
            },
            ReviewEngineArg::Codex => {
                let codex_home = codex_home()?;
                let sandbox = if sandbox == Some("workspace-write") {
                    proto::Sandbox::WorkspaceWrite
                } else {
                    proto::Sandbox::ReadOnly
                };
                (
                    None,
                    Some(StartSessionParams {
                        cwd,
                        executable: executable(codex)?,
                        codex_home,
                        sandbox,
                        prompt: Some(xaya_agentd::task_prompt(task)),
                    }),
                )
            },
        })
    }

    fn review_engines(
        engine: Option<ReviewEngineArg>,
        implementer: &str,
        codex: &Path,
        claude: &Path,
        dual: bool,
    ) -> Result<(proto::ReviewerSpec, Option<proto::ReviewerSpec>)> {
        let primary = review_engine(engine, implementer, codex, claude)?;
        let second = if dual {
            let other = match primary.engine {
                proto::ReviewEngine::Codex => ReviewEngineArg::Claude,
                proto::ReviewEngine::Claude => ReviewEngineArg::Codex,
            };
            Some(review_engine(Some(other), implementer, codex, claude)?)
        } else {
            None
        };
        Ok((primary, second))
    }

    fn review_engine(
        engine: Option<ReviewEngineArg>,
        implementer: &str,
        codex: &Path,
        claude: &Path,
    ) -> Result<proto::ReviewerSpec> {
        let engine = match engine {
            Some(ReviewEngineArg::Codex) => proto::ReviewEngine::Codex,
            Some(ReviewEngineArg::Claude) => proto::ReviewEngine::Claude,
            None if implementer == "codex" => proto::ReviewEngine::Claude,
            None => proto::ReviewEngine::Codex,
        };
        let (path, variable) = match engine {
            proto::ReviewEngine::Codex => (codex, "CODEX_HOME"),
            proto::ReviewEngine::Claude => (claude, "CLAUDE_CONFIG_DIR"),
        };
        let executable = resolve_executable(path)?
            .into_os_string()
            .into_string()
            .map_err(|_| Error::Protocol("executable path must be UTF-8"))?;
        Ok(proto::ReviewerSpec { engine, executable, config_dir: config_dir(variable)? })
    }

    fn review(runtime: &Runtime, command: ReviewAction) -> Result<()> {
        let mut client = Client::connect(runtime)?;
        let review: proto::Review = match command {
            ReviewAction::Send { review_id, command_id: id, no_wait } => {
                let review: proto::Review = client
                    .request(proto::REVIEW_GET, proto::ReviewParams {
                        review_id: review_id.clone(),
                    })?;
                let session_id = review.session_id.clone();
                let get = |client: &mut Client| -> Result<Session> {
                    client.request(proto::SESSION_GET, SessionParams {
                        session_id: session_id.clone(),
                    })
                };
                if review.sent.is_none() && !get(&mut client)?.state.is_active() {
                    eprintln!("resuming {session_id}");
                    let _: Session = client.request(proto::SESSION_RESUME, SessionParams {
                        session_id: session_id.clone(),
                    })?;
                }
                let deadline = Instant::now() + Duration::from_secs(60);
                while review.sent.is_none() && get(&mut client)?.state != proto::SessionState::Ready
                {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "the implementing session did not become ready",
                        ));
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                let id = command_id(id)?;
                eprintln!("command_id={id}");
                let mut receipt: proto::CommandReceipt =
                    client.request(proto::REVIEW_SEND, proto::ReviewSendParams {
                        review_id,
                        command_id: id.clone(),
                    })?;
                let deadline = Instant::now() + Duration::from_secs(60);
                while !no_wait && receipt.status == proto::CommandStatus::Pending {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                    receipt = client.request(proto::SESSION_COMMAND, proto::CommandParams {
                        session_id: session_id.clone(),
                        command_id: receipt.command_id.clone(),
                    })?;
                }
                println!("{}", serde_json::to_string_pretty(&receipt)?);
                return match receipt.status {
                    proto::CommandStatus::Failed | proto::CommandStatus::Unknown => {
                        Err(Error::Protocol("the fixes were not confirmed as sent"))
                    },
                    _ => Ok(()),
                };
            },
            ReviewAction::Dispute { review_id, finding, reason } => {
                let task: serde_json::Value =
                    client.request(proto::REVIEW_DISPUTE, proto::ReviewDisputeParams {
                        review_id,
                        finding,
                        reason,
                    })?;
                println!("{}", serde_json::to_string_pretty(&task)?);
                return Ok(());
            },
            ReviewAction::List { task, session } => {
                let reviews: serde_json::Value =
                    client.request(proto::REVIEW_LIST, proto::ReviewListParams {
                        task_id: task,
                        session_id: session,
                    })?;
                println!("{}", serde_json::to_string_pretty(&reviews)?);
                return Ok(());
            },
            ReviewAction::Show { review_id } => {
                let review: proto::Review =
                    client.request(proto::REVIEW_GET, proto::ReviewParams { review_id })?;
                println!("{}", serde_json::to_string_pretty(&review)?);
                return Ok(());
            },
            ReviewAction::Cancel { review_id } => {
                let review: proto::Review =
                    client.request(proto::REVIEW_CANCEL, proto::ReviewParams { review_id })?;
                println!("{}", serde_json::to_string_pretty(&review)?);
                return Ok(());
            },
            ReviewAction::Run {
                checkpoint_id,
                engine,
                check_id,
                base,
                command_id: id,
                timeout_secs,
                no_wait,
                task,
                no_task,
                codex,
                claude,
            } => {
                let checkpoint: proto::Checkpoint = client
                    .request(proto::CHECKPOINT_GET, proto::CheckpointParams {
                        checkpoint_id: checkpoint_id.clone(),
                    })?;
                let task: Option<proto::Task> = if no_task {
                    None
                } else if let Some(task_id) = task {
                    Some(client.request(proto::TASK_GET, proto::TaskParams { task_id })?)
                } else {
                    client.request(proto::TASK_FOR_SESSION, SessionParams {
                        session_id: checkpoint.session_id.clone(),
                    })?
                };
                let dual = task.as_ref().is_some_and(|task| task.dual_review);
                if let Some(task) = &task {
                    eprintln!("task_id={}", task.id);
                }
                let implementer = if engine.is_none() {
                    client
                        .request::<Session>(proto::SESSION_GET, SessionParams {
                            session_id: checkpoint.session_id.clone(),
                        })
                        .map(|session| session.engine)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let (primary, second_reviewer) =
                    review_engines(engine, &implementer, &codex, &claude, dual)?;
                let id = command_id(id)?;
                eprintln!("command_id={id}");
                let mut review: proto::Review =
                    client.request(proto::REVIEW_START, proto::ReviewStartParams {
                        checkpoint_id,
                        command_id: id,
                        engine: primary.engine,
                        executable: primary.executable,
                        config_dir: primary.config_dir,
                        second_reviewer,
                        check_id,
                        base,
                        timeout_secs,
                        task_id: task.map(|task| task.id),
                    })?;
                let deadline = Instant::now() + Duration::from_secs(timeout_secs + 60);
                while !no_wait && review.status == proto::ReviewStatus::Running {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "review is pending; query review show using the review ID",
                        ));
                    }
                    thread::sleep(Duration::from_millis(500));
                    review = client.request(proto::REVIEW_GET, proto::ReviewParams {
                        review_id: review.id.clone(),
                    })?;
                }
                if no_wait {
                    println!("{}", serde_json::to_string_pretty(&review)?);
                    return Ok(());
                }
                review
            },
        };
        println!("{}", serde_json::to_string_pretty(&review)?);
        match (review.status, review.verdict) {
            (proto::ReviewStatus::Running, _) => Ok(()),
            (_, Some(proto::ReviewVerdict::Approved)) => Ok(()),
            _ => Err(Error::Protocol("the review did not approve this checkpoint")),
        }
    }

    fn command_id(value: Option<String>) -> Result<String> {
        if let Some(value) = value {
            return Ok(value);
        }
        use std::io::Read;
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn resolve_executable(path: &Path) -> Result<PathBuf> {
        if path.components().count() > 1 || path.is_absolute() {
            return Ok(path.canonicalize()?);
        }
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(path))
            .find(|candidate| {
                candidate.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .ok_or(Error::Protocol("engine executable not found on PATH"))?
            .canonicalize()
            .map_err(Into::into)
    }

    fn attach(
        runtime: &Runtime,
        state_dir: Option<&Path>,
        session_id: String,
        prompt: Option<String>,
        allow_resume: bool,
    ) -> Result<()> {
        let mut client =
            Client::ensure_running_with_state(runtime, &env::current_exe()?, state_dir)?;
        let session: Session =
            client.request(proto::SESSION_GET, SessionParams { session_id: session_id.clone() })?;
        if session.engine == "claude" {
            let resumed = !session.state.is_active();
            if resumed {
                if !allow_resume {
                    return Err(Error::Rpc {
                        code: proto::NOT_READY,
                        message: format!(
                            "managed session {} is inactive; explicitly run xaya attach {} to \
                             resume",
                            session.id, session.id
                        ),
                    });
                }
                // Refused while any process of the previous launch is still owned.
                let _: Session = client.request(proto::SESSION_RESUME, SessionParams {
                    session_id: session_id.clone(),
                })?;
            }
            let deadline = Instant::now() + Duration::from_secs(35);
            loop {
                let session: Session = client.request(proto::SESSION_GET, SessionParams {
                    session_id: session_id.clone(),
                })?;
                if !session.state.is_active() {
                    return Err(Error::Protocol(
                        "Claude session is not running; inspect session show",
                    ));
                }
                // A resumed launch keeps its saved identity; wait until Mods verified it.
                if session.native_thread_id.is_some()
                    && !(resumed && session.state == proto::SessionState::Starting)
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(Error::Protocol(
                        "Claude native identity is not ready; inspect session show",
                    ));
                }
                thread::sleep(Duration::from_millis(100));
            }
            let info: AttachInfo =
                client.request(proto::ATTACH_CLAIM, SessionParams { session_id })?;
            let status = Command::new(&info.executable)
                .args(info.args)
                .current_dir(info.cwd)
                .envs(info.environment)
                .status()?;
            // This connection owns the attach lease and must outlive the child TUI.
            drop(client);
            return if status.success() {
                Ok(())
            } else {
                Err(Error::Protocol("Claude attach exited unsuccessfully"))
            };
        }
        if !session.state.is_active() {
            if !allow_resume {
                return Err(Error::Rpc {
                    code: proto::NOT_READY,
                    message: format!(
                        "managed session {} is inactive; explicitly run xaya attach {} to resume",
                        session.id, session.id
                    ),
                });
            }
            let _: Session = client
                .request(proto::SESSION_RESUME, SessionParams { session_id: session_id.clone() })?;
        }
        let info: AttachInfo = client.request(proto::SESSION_ATTACH_INFO, SessionParams {
            session_id: session_id.clone(),
        })?;
        // Supervision decides on the backend's life, so learn it before the TUI starts; a
        // just-started session reports it as soon as its host runs.
        let deadline = Instant::now() + Duration::from_secs(10);
        let backend = loop {
            let session: Session = client
                .request(proto::SESSION_GET, SessionParams { session_id: session_id.clone() })?;
            if session.backend_pid.is_some() || Instant::now() >= deadline {
                break session.backend_pid;
            }
            thread::sleep(Duration::from_millis(50));
        };
        drop(client);
        let mut process = Command::new(&info.executable);
        process.args(info.args).current_dir(info.cwd).env("CODEX_HOME", info.codex_home);
        if let Some(prompt) = prompt {
            process.arg("--").arg(prompt);
        }
        let mut tui = process.spawn()?;
        supervise_tui(runtime, state_dir, &session_id, backend, &mut tui)
    }

    /// How often an attached Codex TUI's session and service are checked.
    const SUPERVISE_INTERVAL: Duration = Duration::from_secs(1);

    /// Stays beside an attached Codex TUI until it exits.
    ///
    /// The TUI reconnects by itself when its connection drops (Codex 0.169), and a restarted
    /// service readopts the backend behind the same endpoint, so a service crash only needs the
    /// service started again: done here unless the backend is known to be gone, with backoff. Once
    /// the session is over, or the service and backend are both gone, the TUI is ended instead of
    /// being left reconnecting to nothing.
    fn supervise_tui(
        runtime: &Runtime,
        state_dir: Option<&Path>,
        session_id: &str,
        mut backend: Option<u32>,
        tui: &mut Child,
    ) -> Result<()> {
        // Ctrl+C and Ctrl+\ reach the terminal's whole foreground group; they're the TUI's keys.
        let _signals = IgnoreTerminalSignals::new();
        // Whoever ends attach means to end the TUI it started: pass the signal on.
        let terminate = Arc::new(AtomicBool::new(false));
        let hangup = Arc::new(AtomicBool::new(false));
        let _forwarded = Forwarded(vec![
            flag::register(SIGTERM, Arc::clone(&terminate))?,
            flag::register(signal_hook::consts::signal::SIGHUP, Arc::clone(&hangup))?,
        ]);
        // Services started here are this process's children; reap them when they exit.
        let mut services: Vec<std::process::Child> = Vec::new();
        let mut retry = Duration::from_secs(1);
        let mut next_restart = Instant::now();
        let mut next_check = Instant::now();
        loop {
            services.retain_mut(|service| !matches!(service.try_wait(), Ok(Some(_))));
            let received = if terminate.load(Ordering::Acquire) {
                Some(libc::SIGTERM)
            } else if hangup.load(Ordering::Acquire) {
                Some(libc::SIGHUP)
            } else {
                None
            };
            if let Some(signal) = received {
                end_child_with(tui, signal);
                return Err(Error::Protocol("attach was terminated; the TUI was ended"));
            }
            if let Some(status) = tui.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(Error::Protocol("Codex exited unsuccessfully"))
                };
            }
            let now = Instant::now();
            if now >= next_check {
                // Learn the backend early: whether to restart a vanished service depends on it.
                next_check = now
                    + if backend.is_some() {
                        SUPERVISE_INTERVAL
                    } else {
                        Duration::from_millis(100)
                    };
                let ended = match Client::connect(runtime) {
                    Ok(mut client) => {
                        retry = Duration::from_secs(1);
                        let params = SessionParams { session_id: session_id.into() };
                        match client.request::<Session>(proto::SESSION_GET, params) {
                            Ok(session) if session.state.is_active() => {
                                backend = session.backend_pid.or(backend);
                                None
                            },
                            Ok(session) => Some(format!(
                                "managed session {session_id} is {}",
                                format!("{:?}", session.state).to_lowercase()
                            )),
                            Err(Error::Rpc { code, .. }) if code == proto::NOT_FOUND => {
                                Some(format!("managed session {session_id} no longer exists"))
                            },
                            Err(_) => None,
                        }
                    },
                    // A backend not yet known may well be running: restarting a service is
                    // harmless, it readopts or settles the session and then idles out.
                    Err(_) if backend.is_none_or(process_alive) => {
                        if now >= next_restart {
                            match Client::ensure_running_spawned(
                                runtime,
                                &env::current_exe()?,
                                state_dir,
                            ) {
                                Ok((_, spawned)) => services.extend(spawned),
                                Err(_) => {
                                    next_restart = now + retry;
                                    retry = (retry * 2).min(Duration::from_secs(30));
                                },
                            }
                        }
                        None
                    },
                    Err(_) => Some("the agent service and the session's backend are gone".into()),
                };
                if let Some(reason) = ended {
                    end_child(tui);
                    eprintln!("xaya-agentd: {reason}; run `xaya attach {session_id}` to resume");
                    return Err(Error::Protocol("the attached session ended"));
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn process_alive(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else { return false };
        // SAFETY: signal 0 only probes whether the process exists.
        let probed = unsafe { libc::kill(pid, 0) };
        probed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Signal handlers registered for this process, unregistered on drop.
    struct Forwarded(Vec<signal_hook::SigId>);

    impl Drop for Forwarded {
        fn drop(&mut self) {
            for id in self.0.drain(..) {
                signal_hook::low_level::unregister(id);
            }
        }
    }

    /// SIGTERM, then SIGKILL after a grace period.
    fn end_child(child: &mut Child) {
        end_child_with(child, libc::SIGTERM);
    }

    /// `signal`, then SIGKILL after a grace period.
    fn end_child_with(child: &mut Child, signal: libc::c_int) {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: the child has not been reaped, so its PID still names it.
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Ignores SIGINT and SIGQUIT in this process while it lives, restoring them on drop.
    ///
    /// Set only after the child is spawned: ignored dispositions survive `exec`.
    struct IgnoreTerminalSignals([libc::sighandler_t; 2]);

    impl IgnoreTerminalSignals {
        fn new() -> Self {
            // SAFETY: replacing a disposition with SIG_IGN installs no handler code.
            unsafe {
                Self([
                    libc::signal(libc::SIGINT, libc::SIG_IGN),
                    libc::signal(libc::SIGQUIT, libc::SIG_IGN),
                ])
            }
        }
    }

    impl Drop for IgnoreTerminalSignals {
        fn drop(&mut self) {
            // SAFETY: restores the dispositions `new` replaced.
            unsafe {
                libc::signal(libc::SIGINT, self.0[0]);
                libc::signal(libc::SIGQUIT, self.0[1]);
            }
        }
    }

    pub fn run() -> Result<()> {
        let options = Options::parse();
        // The PTY host forks; dispatch it before anything else touches the process.
        if let Some(Action::PtyHost { dir, rows, cols, relay_fd, relay_to, argv }) =
            &options.command
        {
            let size = xaya_agentd::pty_host::Size { rows: *rows, cols: *cols };
            let relay = relay_fd.zip(relay_to.as_deref());
            return xaya_agentd::pty_host::serve(dir, size, argv, relay);
        }
        let runtime = Runtime::prepare(options.runtime_dir)?;
        let state_dir = options.state_dir;
        match options.command.unwrap_or(Action::Status { json: false, no_start: false }) {
            Action::Run(options) => {
                if options.engine == "claude" {
                    if options.sandbox.is_some() {
                        return Err(Error::Protocol(
                            "Claude uses manual native permissions; Codex sandbox overrides do \
                             not apply",
                        ));
                    }
                    let prompt = options
                        .prompt
                        .ok_or(Error::Protocol("Claude requires an explicit first prompt"))?;
                    let text_path = |path: PathBuf| -> Result<String> {
                        path.canonicalize()?
                            .into_os_string()
                            .into_string()
                            .map_err(|_| Error::Protocol("Claude paths must be UTF-8"))
                    };
                    let params = proto::StartClaudeParams {
                        cwd: text_path(options.cwd)?,
                        executable: text_path(resolve_executable(&options.claude)?)?,
                        config_dir: config_dir("CLAUDE_CONFIG_DIR")?,
                        prompt,
                    };
                    let mut client = Client::ensure_running_with_state(
                        &runtime,
                        &env::current_exe()?,
                        state_dir.as_deref(),
                    )?;
                    let session: Session = client.request(proto::CLAUDE_START, params)?;
                    drop(client);
                    if options.json {
                        println!("{}", serde_json::to_string_pretty(&session)?);
                    } else if options.detach {
                        println!("{}", session.id);
                    } else {
                        attach(&runtime, state_dir.as_deref(), session.id, None, false)?;
                    }
                    return Ok(());
                }
                if options.prompt.is_some() && (options.detach || options.json) {
                    return Err(Error::Protocol("a first prompt requires an attached native TUI"));
                }
                let executable = resolve_executable(&options.codex)?;
                let cwd = options.cwd.canonicalize()?;
                let codex_home = match env::var_os("CODEX_HOME") {
                    Some(home) => PathBuf::from(home),
                    None => PathBuf::from(
                        env::var_os("HOME").ok_or(Error::Protocol("HOME is not set"))?,
                    )
                    .join(".codex"),
                };
                let codex_home = std::path::absolute(codex_home)?;
                let text_path = |path: &Path| {
                    path.to_str()
                        .map(str::to_owned)
                        .ok_or(Error::Protocol("managed session paths must be UTF-8"))
                };
                let params = StartSessionParams {
                    cwd: text_path(&cwd)?,
                    executable: text_path(&executable)?,
                    codex_home: text_path(&codex_home)?,
                    sandbox: if options.sandbox.as_deref() == Some("workspace-write") {
                        proto::Sandbox::WorkspaceWrite
                    } else {
                        proto::Sandbox::ReadOnly
                    },
                    prompt: None,
                };
                let mut client = Client::ensure_running_with_state(
                    &runtime,
                    &env::current_exe()?,
                    state_dir.as_deref(),
                )?;
                let session: Session = client.request(proto::SESSION_START, params)?;
                drop(client);
                if options.json {
                    println!("{}", serde_json::to_string_pretty(&session)?);
                } else if options.detach {
                    println!("{}", session.id);
                } else {
                    eprintln!(
                        "XaYa session {} (backend continues after the TUI closes)",
                        session.id
                    );
                    attach(&runtime, state_dir.as_deref(), session.id, options.prompt, true)?;
                }
                Ok(())
            },
            Action::Attach { session_id } => {
                attach(&runtime, state_dir.as_deref(), session_id, None, true)
            },
            Action::Restore { native_thread_id } => {
                let mut client = Client::ensure_running_with_state(
                    &runtime,
                    &env::current_exe()?,
                    state_dir.as_deref(),
                )?;
                let sessions: Vec<Session> =
                    client.request(proto::SESSION_LIST, serde_json::Value::Null)?;
                drop(client);
                if let Some(session) = sessions
                    .into_iter()
                    .find(|session| session.native_thread_id.as_deref() == Some(&native_thread_id))
                {
                    attach(&runtime, state_dir.as_deref(), session.id, None, false)
                } else {
                    Err(Command::new("codex").args(["resume", &native_thread_id]).exec().into())
                }
            },
            Action::Checkpoint { command } => {
                let mut client = Client::connect(&runtime)?;
                let (mut cp, wait, json) = match command {
                    CheckpointAction::Create { session_id, command_id: id, no_wait, json } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        (
                            client.request::<proto::Checkpoint>(
                                proto::CHECKPOINT_CREATE,
                                proto::CheckpointCreateParams { session_id, command_id: id },
                            )?,
                            !no_wait,
                            json,
                        )
                    },
                    CheckpointAction::Show { checkpoint_id } => (
                        client.request::<proto::Checkpoint>(
                            proto::CHECKPOINT_GET,
                            proto::CheckpointParams { checkpoint_id },
                        )?,
                        false,
                        true,
                    ),
                };
                let deadline = Instant::now() + Duration::from_secs(40);
                if wait {
                    eprintln!("checkpoint_id={}", cp.id);
                }
                while wait && cp.status == proto::CheckpointStatus::Creating {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "checkpoint is still pending; query checkpoint show using its ID",
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                    cp = client.request(proto::CHECKPOINT_GET, proto::CheckpointParams {
                        checkpoint_id: cp.id.clone(),
                    })?;
                }
                if json {
                    println!("{}", serde_json::to_string_pretty(&cp)?);
                } else {
                    println!("{} {:?} {}", cp.id, cp.status, cp.commit.as_deref().unwrap_or(""));
                }
                if wait && cp.status != proto::CheckpointStatus::Ready {
                    return Err(Error::Rpc {
                        code: proto::ENGINE_ERROR,
                        message: cp.error.unwrap_or_else(|| "checkpoint failed".into()),
                    });
                }
                Ok(())
            },
            Action::Land { command } => {
                let mut client = Client::connect(&runtime)?;
                let (mut preview, wait) = match command {
                    LandAction::Preview {
                        checkpoint_id,
                        check_id,
                        target,
                        command_id: id,
                        no_wait,
                    } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let preview: proto::LandPreview =
                            client.request(proto::LAND_PREVIEW, proto::LandPreviewParams {
                                checkpoint_id,
                                check_id,
                                target_branch: target,
                                command_id: id,
                            })?;
                        eprintln!("preview_id={}", preview.id);
                        (preview, !no_wait)
                    },
                    LandAction::Verify { preview_id, no_wait } => (
                        client.request(proto::LAND_VERIFY, proto::LandParams { preview_id })?,
                        !no_wait,
                    ),
                    LandAction::Run { preview_id } => {
                        let result: proto::LandPreview =
                            client.request(proto::LAND_RUN, proto::LandParams { preview_id })?;
                        println!("{}", serde_json::to_string_pretty(&result)?);
                        if result.status != proto::LandStatus::Landed {
                            return Err(Error::Rpc {
                                code: proto::CONFLICT,
                                message: result
                                    .error
                                    .unwrap_or_else(|| "landing did not complete".into()),
                            });
                        }
                        return Ok(());
                    },
                    LandAction::Reconcile { preview_id } => {
                        let result: proto::LandPreview = client
                            .request(proto::LAND_RECONCILE, proto::LandParams { preview_id })?;
                        println!("{}", serde_json::to_string_pretty(&result)?);
                        if matches!(
                            result.landing_outcome,
                            Some(proto::LandOutcome::Unknown | proto::LandOutcome::NeedsAttention)
                        ) {
                            return Err(Error::Protocol("landing requires manual inspection"));
                        }
                        return Ok(());
                    },
                    LandAction::Resume { preview_id } => {
                        let result: proto::LandPreview =
                            client.request(proto::LAND_RESUME, proto::LandParams { preview_id })?;
                        println!("{}", serde_json::to_string_pretty(&result)?);
                        if result.status != proto::LandStatus::Landed {
                            return Err(Error::Rpc {
                                code: proto::CONFLICT,
                                message: result
                                    .error
                                    .unwrap_or_else(|| "landing was not resumed".into()),
                            });
                        }
                        return Ok(());
                    },
                    LandAction::Show { preview_id } => {
                        (client.request(proto::LAND_GET, proto::LandParams { preview_id })?, false)
                    },
                    LandAction::Recheck { preview_id } => {
                        let recheck: proto::LandRecheck = client
                            .request(proto::LAND_RECHECK, proto::LandParams { preview_id })?;
                        println!("{}", serde_json::to_string_pretty(&recheck)?);
                        if !recheck.valid {
                            return Err(Error::Rpc {
                                code: proto::CONFLICT,
                                message: recheck
                                    .reason
                                    .unwrap_or_else(|| "landing preconditions failed".into()),
                            });
                        }
                        return Ok(());
                    },
                };
                let deadline = Instant::now() + Duration::from_secs(3700);
                while wait
                    && matches!(
                        preview.status,
                        proto::LandStatus::Creating | proto::LandStatus::Verifying
                    )
                {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "landing preview is pending; query land show using its ID",
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                    preview = client.request(proto::LAND_GET, proto::LandParams {
                        preview_id: preview.id.clone(),
                    })?;
                }
                println!("{}", serde_json::to_string_pretty(&preview)?);
                if wait && (preview.status != proto::LandStatus::Ready || preview.stale) {
                    return Err(Error::Rpc {
                        code: proto::CONFLICT,
                        message: preview
                            .error
                            .unwrap_or_else(|| "landing preview is not ready".into()),
                    });
                }
                Ok(())
            },
            Action::Project { command } => {
                let mut client = Client::ensure_running_with_state(
                    &runtime,
                    &env::current_exe()?,
                    state_dir.as_deref(),
                )?;
                let info: proto::ProjectInfo = match command {
                    ProjectAction::Show { cwd } => {
                        client.request(proto::PROJECT_GET, proto::ProjectParams {
                            cwd: cwd
                                .canonicalize()?
                                .to_str()
                                .ok_or(Error::Protocol("project path must be UTF-8"))?
                                .into(),
                        })?
                    },
                    ProjectAction::Trust { cwd, hash } => {
                        client.request(proto::PROJECT_TRUST, proto::ProjectTrustParams {
                            cwd: cwd
                                .canonicalize()?
                                .to_str()
                                .ok_or(Error::Protocol("project path must be UTF-8"))?
                                .into(),
                            config_hash: hash,
                        })?
                    },
                };
                println!("{}", serde_json::to_string_pretty(&info)?);
                Ok(())
            },
            Action::Review { command } => review(&runtime, command),
            Action::Task { command } => task(&runtime, state_dir.as_deref(), command),
            Action::Check { command } => {
                let mut client = Client::connect(&runtime)?;
                let (mut check, wait) = match command {
                    CheckAction::Latest { session_id } => {
                        let check: Option<proto::Check> = client
                            .request(proto::CHECK_LATEST, proto::SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&check)?);
                        return Ok(());
                    },
                    CheckAction::Project {
                        checkpoint_id,
                        command_id: id,
                        config_hash,
                        no_wait,
                    } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let config_hash = match config_hash {
                            Some(hash) => hash,
                            None => {
                                let cp: proto::Checkpoint = client.request(
                                    proto::CHECKPOINT_GET,
                                    proto::CheckpointParams {
                                        checkpoint_id: checkpoint_id.clone(),
                                    },
                                )?;
                                let info: proto::ProjectInfo = client
                                    .request(proto::PROJECT_GET, proto::ProjectParams {
                                        cwd: cp.cwd,
                                    })?;
                                info.config_hash
                            },
                        };
                        let check: proto::Check =
                            client.request(proto::CHECK_PROJECT, proto::ProjectCheckParams {
                                checkpoint_id,
                                command_id: id,
                                config_hash,
                            })?;
                        eprintln!("check_id={}", check.id);
                        (check, !no_wait)
                    },
                    CheckAction::Run {
                        checkpoint_id,
                        command_id: id,
                        timeout_secs,
                        no_wait,
                        argv,
                    } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let check: proto::Check =
                            client.request(proto::CHECK_RUN, proto::CheckRunParams {
                                checkpoint_id,
                                command_id: id,
                                argv,
                                timeout_secs,
                            })?;
                        eprintln!("check_id={}", check.id);
                        (check, !no_wait)
                    },
                    CheckAction::Bind { check_id, command_id: id, path, no_wait } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let mut check: proto::Check =
                            client.request(proto::CHECK_BIND, proto::CheckBindParams {
                                check_id: check_id.clone(),
                                command_id: id,
                                path,
                            })?;
                        // Baseline plus two canaries, each bounded by the original budget.
                        let deadline = Instant::now()
                            + Duration::from_secs(
                                (check.timeout_secs + 30 * (check.steps.len() as u64 + 2)) * 3 + 10,
                            );
                        while !no_wait && check.binding.status == proto::BindingStatus::Running {
                            if Instant::now() >= deadline {
                                return Err(Error::Protocol(
                                    "binding is pending; query check show using the check ID",
                                ));
                            }
                            thread::sleep(Duration::from_millis(200));
                            check = client.request(proto::CHECK_GET, proto::CheckParams {
                                check_id: check_id.clone(),
                            })?;
                        }
                        println!("{}", serde_json::to_string_pretty(&check)?);
                        let settled = check.binding.status != proto::BindingStatus::Running;
                        if !no_wait
                            && settled
                            && check.binding.status != proto::BindingStatus::Proved
                        {
                            return Err(Error::Rpc {
                                code: proto::ENGINE_ERROR,
                                message: format!(
                                    "binding {:?}: {}",
                                    check.binding.status,
                                    check.binding.reason.unwrap_or_default()
                                ),
                            });
                        }
                        return Ok(());
                    },
                    CheckAction::Show { check_id } => {
                        (client.request(proto::CHECK_GET, proto::CheckParams { check_id })?, false)
                    },
                    CheckAction::Cleanup { check_id } => {
                        let check: proto::Check = client
                            .request(proto::CHECK_CLEANUP, proto::CheckParams { check_id })?;
                        println!("{}", serde_json::to_string_pretty(&check)?);
                        if check.integration_cleanup_pending {
                            return Err(Error::Rpc {
                                code: proto::CONFLICT,
                                message: check
                                    .integration_cleanup_error
                                    .unwrap_or_else(|| "integration cleanup is pending".into()),
                            });
                        }
                        return Ok(());
                    },
                    CheckAction::Cancel { check_id } => (
                        client.request(proto::CHECK_CANCEL, proto::CheckParams { check_id })?,
                        false,
                    ),
                    CheckAction::Log { check_id, offset } => {
                        let log: proto::CheckLog =
                            client.request(proto::CHECK_LOG, proto::CheckLogParams {
                                check_id,
                                offset,
                            })?;
                        println!("{}", serde_json::to_string_pretty(&log)?);
                        return Ok(());
                    },
                };
                let deadline = Instant::now()
                    + Duration::from_secs(
                        check.timeout_secs + 30 * (check.steps.len() as u64 + 2) + 10,
                    );
                while wait && check.status == proto::CheckStatus::Running {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "verification is pending; query check show using its ID",
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                    check = client.request(proto::CHECK_GET, proto::CheckParams {
                        check_id: check.id.clone(),
                    })?;
                }
                println!("{}", serde_json::to_string_pretty(&check)?);
                if wait && (check.status != proto::CheckStatus::Succeeded || check.stale) {
                    return Err(Error::Rpc {
                        code: proto::ENGINE_ERROR,
                        message: check
                            .error
                            .unwrap_or_else(|| format!("verification {:?}", check.status)),
                    });
                }
                Ok(())
            },
            Action::Session { command } => {
                let mut client = Client::connect(&runtime)?;
                let stop = matches!(command, SessionAction::Stop { .. });
                match command {
                    SessionAction::Resume { session_id } => {
                        let session: Session =
                            client.request(proto::SESSION_RESUME, SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&session)?);
                    },
                    SessionAction::Board { watch: false } => {
                        let rows: Vec<proto::ManagedRow> =
                            client.request(proto::BOARD_SNAPSHOT, serde_json::Value::Null)?;
                        println!("{}", serde_json::to_string(&rows)?);
                    },
                    SessionAction::Board { watch: true } => {
                        use std::io::Write;
                        println!("{}", serde_json::to_string(&client.watch_board()?)?);
                        std::io::stdout().flush()?;
                        loop {
                            println!("{}", serde_json::to_string(&client.next_board()?)?);
                            std::io::stdout().flush()?;
                        }
                    },
                    SessionAction::Send { session_id, text, command_id: id } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let receipt: proto::CommandReceipt =
                            client.request(proto::SESSION_SEND, proto::SendParams {
                                session_id,
                                text,
                                command_id: id,
                            })?;
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    },
                    SessionAction::Cancel { session_id, turn_id, command_id: id } => {
                        let turn_id = match turn_id {
                            Some(id) => id,
                            None => {
                                let session: Session = client
                                    .request(proto::SESSION_GET, SessionParams {
                                        session_id: session_id.clone(),
                                    })?;
                                session
                                    .turn_id
                                    .ok_or(Error::Protocol("session has no active turn"))?
                            },
                        };
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let receipt: proto::CommandReceipt =
                            client.request(proto::SESSION_CANCEL, proto::CancelParams {
                                session_id,
                                turn_id,
                                command_id: id,
                            })?;
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    },
                    SessionAction::Command { session_id, command_id } => {
                        let receipt: proto::CommandReceipt =
                            client.request(proto::SESSION_COMMAND, proto::CommandParams {
                                session_id,
                                command_id,
                            })?;
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    },
                    SessionAction::EndTools { session_id } => {
                        let ended: proto::ToolsEnded = client
                            .request(proto::SESSION_END_TOOLS, SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&ended)?);
                    },
                    SessionAction::History { session_id, mut after } => {
                        use std::io::Write;
                        loop {
                            let page: proto::EventHistory =
                                client.request(proto::EVENTS_HISTORY, proto::HistoryParams {
                                    session_id: session_id.clone(),
                                    after_seq: after,
                                })?;
                            for event in &page.events {
                                println!("{}", serde_json::to_string(event)?);
                            }
                            match page.events.last() {
                                Some(last) if last.seq < page.last_seq => after = last.seq,
                                _ => break,
                            }
                        }
                        std::io::stdout().flush()?;
                    },
                    SessionAction::Watch { session_id, after } => {
                        use std::io::Write;
                        let snapshot = client.subscribe_after(&session_id, after)?;
                        println!("{}", serde_json::to_string(&snapshot)?);
                        std::io::stdout().flush()?;
                        loop {
                            println!("{}", serde_json::to_string(&client.next_event()?)?);
                            std::io::stdout().flush()?;
                        }
                    },
                    SessionAction::Approvals { session_id } => {
                        let snapshot: proto::EventSnapshot =
                            client.request(proto::APPROVAL_LIST, SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&snapshot)?);
                    },
                    SessionAction::Respond {
                        session_id,
                        approval_id,
                        decision,
                        command_id: id,
                    } => {
                        let id = command_id(id)?;
                        eprintln!("command_id={id}");
                        let receipt: proto::CommandReceipt =
                            client.request(proto::APPROVAL_RESPOND, proto::RespondParams {
                                session_id,
                                approval_id,
                                command_id: id,
                                decision: match decision.as_str() {
                                    "accept" => proto::ApprovalDecision::Accept,
                                    "cancel" => proto::ApprovalDecision::Cancel,
                                    _ => proto::ApprovalDecision::Decline,
                                },
                            })?;
                        println!("{}", serde_json::to_string_pretty(&receipt)?);
                    },
                    SessionAction::List { json } => {
                        let sessions: Vec<Session> =
                            client.request(proto::SESSION_LIST, serde_json::Value::Null)?;
                        if json {
                            println!("{}", serde_json::to_string_pretty(&sessions)?);
                        } else {
                            for session in sessions {
                                println!(
                                    "{}  {:?}  {}  {}",
                                    session.id,
                                    session.state,
                                    if session.attached { "attached" } else { "detached" },
                                    session.cwd
                                );
                            }
                        }
                    },
                    SessionAction::Show { session_id } | SessionAction::Stop { session_id } => {
                        let method = if stop { proto::SESSION_STOP } else { proto::SESSION_GET };
                        let session: Session =
                            client.request(method, SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&session)?);
                    },
                    SessionAction::AttachInfo { session_id } => {
                        let info: AttachInfo = client
                            .request(proto::SESSION_ATTACH_INFO, SessionParams { session_id })?;
                        println!("{}", serde_json::to_string_pretty(&info)?);
                    },
                }
                Ok(())
            },
            Action::PtyHost { .. } => Err(Error::Protocol("PTY host was dispatched earlier")),
            Action::PtyAttach { dir } => xaya_agentd::pty_host::interactive(&dir),
            Action::Serve { idle_timeout_secs } => {
                if idle_timeout_secs == 0 {
                    return Err(Error::Protocol("idle timeout must be positive"));
                }
                let stopping = Arc::new(AtomicBool::new(false));
                flag::register(SIGINT, Arc::clone(&stopping))?;
                flag::register(SIGTERM, Arc::clone(&stopping))?;
                xaya_agentd::server::serve_with_state(
                    &runtime,
                    Duration::from_secs(idle_timeout_secs),
                    stopping,
                    state_dir,
                )
            },
            action @ (Action::Start { .. } | Action::Status { .. }) => {
                let (json, no_start) = match action {
                    Action::Start { json } => (json, false),
                    Action::Status { json, no_start } => (json, no_start),
                    _ => return Err(Error::Protocol("invalid status action")),
                };
                let mut client = if no_start {
                    Client::connect(&runtime)?
                } else {
                    Client::ensure_running_with_state(
                        &runtime,
                        &env::current_exe()?,
                        state_dir.as_deref(),
                    )?
                };
                let status = client.status()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    println!(
                        "agentd running (pid {}, {} managed sessions, uptime {}s)",
                        status.pid,
                        status.active_sessions,
                        status.uptime_ms / 1000
                    );
                }
                Ok(())
            },
            Action::Stop { json } => {
                let mut client = match Client::connect(&runtime) {
                    Ok(client) => client,
                    Err(Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        if json {
                            println!("{{\"accepted\":false}}");
                        } else {
                            println!("agentd is not running");
                        }
                        return Ok(());
                    },
                    Err(error) => return Err(error),
                };
                let result = client.shutdown()?;
                drop(client);
                let deadline = Instant::now() + Duration::from_secs(5);
                while runtime.socket().exists() {
                    if Instant::now() >= deadline {
                        return Err(Error::Protocol(
                            "service acknowledged shutdown but has not stopped",
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                if json {
                    println!("{}", serde_json::to_string(&result)?);
                } else {
                    println!("agentd stopped");
                }
                Ok(())
            },
        }
    }
}

fn main() {
    #[cfg(unix)]
    if let Err(error) = unix::run() {
        eprintln!("xaya-agentd: {error}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("xaya-agentd currently requires a Unix platform");
        std::process::exit(1);
    }
}
