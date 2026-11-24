//! Blocking companion-CLI calls stay off the window thread. The GUI links protocol types only.
//!
//! While the managed view is open, the board arrives pushed from one `session board --watch`
//! child; the one-second refresh then only fetches the inspected detail. When the watch is down
//! (no service, or it ended), the refresh polls the board as before.
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;
use xaya_agent_proto::{
    ApprovalDecision, CommandReceipt, CommandStatus, EventSnapshot, ManagedRow,
};

use super::{ManagedAction, ManagedUpdate};
use crate::event::{Event, EventType};

enum Request {
    /// (inspected session, its detail view, poll the board too)
    Refresh(Option<String>, super::DetailView, bool),
    Action(ManagedAction),
}

/// The watch runs only while refreshes keep coming, i.e. while the managed view is open.
const WATCH_IDLE: Duration = Duration::from_secs(3);
/// Pause before starting a watch again after one ended (e.g. no service yet).
const WATCH_RETRY: Duration = Duration::from_secs(1);

pub struct Bridge {
    sender: mpsc::SyncSender<Request>,
    refreshing: Arc<AtomicBool>,
    watch: Arc<Watch>,
}

struct Watch {
    last_refresh: Mutex<Instant>,
    /// A board line arrived from the running watch; refreshes need not poll the board.
    live: AtomicBool,
    closed: AtomicBool,
    idle: Duration,
    retry: Duration,
}

impl Watch {
    fn new(idle: Duration, retry: Duration) -> Self {
        Self {
            last_refresh: Mutex::new(Instant::now()),
            live: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            idle,
            retry,
        }
    }

    fn touch(&self) {
        if let Ok(mut at) = self.last_refresh.lock() {
            *at = Instant::now();
        }
    }

    fn wanted(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self.last_refresh.lock().map(|at| at.elapsed() < self.idle).unwrap_or(false)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.watch.closed.store(true, Ordering::Release);
    }
}

/// Keep one board watch child running while wanted; restart it after it ends. `deliver`
/// returns false once nobody can receive boards any more, which ends the loop.
fn watch_loop(
    watch: Arc<Watch>,
    command: impl Fn() -> Option<Command>,
    deliver: impl Fn(Vec<ManagedRow>) -> bool + Send + Sync + 'static,
) {
    let deliver = Arc::new(deliver);
    while !watch.closed.load(Ordering::Acquire) {
        if !watch.wanted() {
            thread::sleep(Duration::from_millis(50).min(watch.idle));
            continue;
        }
        let Some(mut command) = command() else { return };
        let child =
            command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        let Ok(mut child) = child else {
            thread::sleep(watch.retry);
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        // This child's generation: once it is ended, whatever its pipe still yields (e.g. from a
        // descendant holding it open) is neither delivered nor waited for.
        let current = Arc::new(AtomicBool::new(true));
        let reader = {
            let (watch, deliver, current) =
                (Arc::clone(&watch), Arc::clone(&deliver), Arc::clone(&current));
            thread::spawn(move || {
                // One board per line; a line is bounded like any other service reply.
                for line in BufReader::new(stdout.take(64 << 20)).lines() {
                    let Ok(rows) = line.map_err(drop).and_then(|line| {
                        serde_json::from_str::<Vec<ManagedRow>>(&line).map_err(drop)
                    }) else {
                        break;
                    };
                    if !current.load(Ordering::Acquire) {
                        break;
                    }
                    watch.live.store(true, Ordering::Release);
                    if !deliver(rows) {
                        watch.closed.store(true, Ordering::Release);
                        break;
                    }
                }
                if current.load(Ordering::Acquire) {
                    watch.live.store(false, Ordering::Release);
                }
            })
        };
        // Ended by: end of its output, the child exiting (even if a descendant still holds the
        // pipe), or nobody wanting the board any more.
        while !reader.is_finished()
            && watch.wanted()
            && child.try_wait().is_ok_and(|status| status.is_none())
        {
            thread::sleep(Duration::from_millis(20));
        }
        current.store(false, Ordering::Release);
        watch.live.store(false, Ordering::Release);
        let _ = child.kill();
        let _ = child.wait();
        if reader.is_finished() {
            let _ = reader.join();
        }
        thread::sleep(watch.retry);
    }
}

impl Bridge {
    pub fn new(proxy: EventLoopProxy<Event>, window_id: WindowId) -> Self {
        let (sender, receiver) = mpsc::sync_channel(8);
        let refreshing = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&refreshing);
        let watch = Arc::new(Watch::new(WATCH_IDLE, WATCH_RETRY));
        {
            let (watch, proxy) = (Arc::clone(&watch), proxy.clone());
            let command = || {
                let executable = std::env::current_exe().ok()?.with_file_name("xaya-agentd");
                let mut command = Command::new(executable);
                command.args(["session", "board", "--watch"]);
                Some(command)
            };
            let deliver = move |rows| {
                let update = ManagedUpdate { rows: Some(rows), ..Default::default() };
                let event = Event::new(EventType::ManagedPanel(Box::new(update)), window_id);
                proxy.send_event(event).is_ok()
            };
            thread::spawn(move || watch_loop(watch, command, deliver));
        }
        thread::spawn(move || {
            let mut pending: Option<(String, String)> = None;
            while let Ok(request) = receiver.recv() {
                let update = match request {
                    Request::Refresh(inspect, view, poll) => {
                        let mut update = ManagedUpdate::default();
                        if poll {
                            match call::<Vec<ManagedRow>>(&["session", "board"]) {
                                Ok(rows) => update.rows = Some(rows),
                                Err(error) => update.error = Some(error),
                            }
                        }
                        if let Some(id) = inspect {
                            if view == super::DetailView::Acceptance {
                                match acceptance(&id) {
                                    Ok(loaded) => update.acceptance = Some((id, loaded)),
                                    Err(error) => update.error = Some(error),
                                }
                            } else if view == super::DetailView::Review {
                                match latest_review(&id) {
                                    Ok((review, attention, dependencies)) => {
                                        update.dependencies = Some((id.clone(), dependencies));
                                        update.task_attention = Some((id.clone(), attention));
                                        update.review = Some((id, review));
                                    },
                                    Err(error) => update.error = Some(error),
                                }
                            } else if view == super::DetailView::Checks {
                                match call::<Option<xaya_agent_proto::Check>>(&[
                                    "check", "latest", &id,
                                ]) {
                                    Ok(check) => update.verification = Some((id, check)),
                                    Err(error) => update.error = Some(error),
                                }
                            } else {
                                match call::<EventSnapshot>(&["session", "approvals", &id]) {
                                    Ok(detail) => update.detail = Some(detail),
                                    Err(error) => update.error = Some(error),
                                }
                            }
                        }
                        if let Some((session, id)) = &pending
                            && let Ok(receipt) =
                                call::<CommandReceipt>(&["session", "command", session, id])
                            && receipt.status != CommandStatus::Pending
                        {
                            update.notice = Some(receipt_notice(&receipt));
                            pending = None;
                        }
                        flag.store(false, Ordering::Release);
                        update
                    },
                    Request::Action(action) => {
                        let control = !matches!(
                            &action,
                            ManagedAction::ProjectBoard(_)
                                | ManagedAction::Inspect(_)
                                | ManagedAction::InspectChecks(_)
                                | ManagedAction::InspectReview(_)
                                | ManagedAction::CancelCheck(_)
                                | ManagedAction::EndTools { .. }
                                | ManagedAction::Delegate { .. }
                                | ManagedAction::InspectTask(_)
                                | ManagedAction::Land { .. }
                                | ManagedAction::Revise { .. }
                        );
                        let (update, waiting) = action_update(action);
                        if control {
                            pending = waiting;
                        }
                        update
                    },
                };
                if proxy
                    .send_event(Event::new(EventType::ManagedPanel(Box::new(update)), window_id))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { sender, refreshing, watch }
    }

    pub fn refresh(&self, inspect: Option<String>, view: super::DetailView) {
        self.watch.touch();
        // The pushed board is current; without a live watch, poll it. The refresh still runs
        // for the inspected detail and pending command receipts (spawning nothing otherwise).
        let poll = !self.watch.live.load(Ordering::Acquire);
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.sender.try_send(Request::Refresh(inspect, view, poll)).is_err() {
            self.refreshing.store(false, Ordering::Release);
        }
    }

    pub fn action(&self, action: ManagedAction) -> bool {
        self.sender.try_send(Request::Action(action)).is_ok()
    }
}

fn action_update(action: ManagedAction) -> (ManagedUpdate, Option<(String, String)>) {
    let mut update = ManagedUpdate { action_done: true, ..Default::default() };
    if let ManagedAction::ProjectBoard(path) = action {
        match call(&["task", "board", &path.to_string_lossy()]) {
            Ok(board) => update.board = Some(board),
            Err(error) => update.error = Some(error),
        }
        return (update, None);
    }
    if let ManagedAction::Delegate { cwd, goal, request_id, implementer } = action {
        let cwd = cwd.to_string_lossy();
        // Creating the task, its workspace plan and its drive takes a few Git calls.
        let result = call_in::<xaya_agent_proto::Task>(
            &[
                "task",
                "delegate",
                "--request-id",
                &request_id,
                "--cwd",
                &cwd,
                "--engine",
                implementer.name(),
                "--",
                &goal,
            ],
            Duration::from_secs(30),
            login_path(),
        );
        update.submitted = Some((
            request_id,
            result.map(|task| format!(" Handed over · {} · {}", task.title, task.id)),
        ));
        if let Ok(board) = call(&["task", "board", &cwd]) {
            update.board = Some(board);
        }
        return (update, None);
    }
    if let ManagedAction::InspectTask(id) = action {
        match acceptance(&id) {
            Ok(loaded) => update.acceptance = Some((id, loaded)),
            Err(error) => update.error = Some(error),
        }
        return (update, None);
    }
    if let ManagedAction::Revise { task_id, request_id, feedback } = action {
        let result = call::<xaya_agent_proto::Task>(&[
            "task",
            "revise",
            &task_id,
            "--request-id",
            &request_id,
            "--",
            &feedback,
        ]);
        update.submitted = Some((
            request_id,
            result.map(|_| " Sent · it will be verified and reviewed again".to_owned()),
        ));
        if let Ok(loaded) = acceptance(&task_id) {
            update.acceptance = Some((task_id, loaded));
        }
        return (update, None);
    }
    if let ManagedAction::Land { task_id, request_id } = action {
        match call::<xaya_agent_proto::Task>(&[
            "task",
            "land",
            &task_id,
            "--request-id",
            &request_id,
        ]) {
            Ok(_) => update.notice = Some(" Landing in the background…".into()),
            // Not knowing whether the service took it, asking again sends the same request.
            Err(error) => update.error = Some(format!(" Not landed: {error}")),
        }
        if let Ok(loaded) = acceptance(&task_id) {
            update.acceptance = Some((task_id, loaded));
        }
        return (update, None);
    }
    if let ManagedAction::Inspect(id) = action {
        match call(&["session", "approvals", &id]) {
            Ok(detail) => update.detail = Some(detail),
            Err(error) => update.error = Some(error),
        }
        return (update, None);
    }
    if let ManagedAction::InspectReview(session_id) = action {
        match latest_review(&session_id) {
            Ok((review, attention, dependencies)) => {
                update.dependencies = Some((session_id.clone(), dependencies));
                update.task_attention = Some((session_id.clone(), attention));
                update.review = Some((session_id, review));
            },
            Err(error) => update.error = Some(error),
        }
        return (update, None);
    }
    if let ManagedAction::InspectChecks(session_id) = action {
        match call::<Option<xaya_agent_proto::Check>>(&["check", "latest", &session_id]) {
            Ok(check) => update.verification = Some((session_id, check)),
            Err(error) => update.error = Some(error),
        }
        return (update, None);
    }
    if let ManagedAction::EndTools { session_id } = action {
        // Ending tools waits for them to exit (SIGTERM, then SIGKILL): allow longer.
        match call_with::<xaya_agent_proto::ToolsEnded>(
            &["session", "end-tools", &session_id],
            Duration::from_secs(20),
        ) {
            Ok(ended) => {
                update.notice = Some(format!(
                    " Ended {} tool process{} · the session keeps running",
                    ended.ended,
                    if ended.ended == 1 { "" } else { "es" }
                ))
            },
            Err(error) => update.error = Some(format!("Tools not ended: {error}")),
        }
        return (update, None);
    }
    if let ManagedAction::CancelCheck(check_id) = action {
        match call::<xaya_agent_proto::Check>(&["check", "cancel", &check_id]) {
            Ok(check) => update.verification = Some((check.session_id.clone(), Some(check))),
            Err(error) => {
                update.error =
                    Some(format!("Cancellation uncertain; query check {check_id}: {error}"))
            },
        }
        return (update, None);
    }
    static IDS: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let id =
        format!("panel-{}-{stamp}-{}", std::process::id(), IDS.fetch_add(1, Ordering::Relaxed));
    let (session_id, result) = match action {
        ManagedAction::Respond { session_id, approval_id, decision } => {
            let decision = match decision {
                ApprovalDecision::Accept => "accept",
                ApprovalDecision::Decline => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            let result = call::<CommandReceipt>(&[
                "session",
                "respond",
                &session_id,
                &approval_id,
                decision,
                "--command-id",
                &id,
            ]);
            (session_id, result)
        },
        ManagedAction::Cancel { session_id, turn_id } => {
            let result = call::<CommandReceipt>(&[
                "session",
                "cancel",
                &session_id,
                "--turn-id",
                &turn_id,
                "--command-id",
                &id,
            ]);
            (session_id, result)
        },
        ManagedAction::SendReview { session_id, review_id } => {
            // A stopped session is resumed first, which takes a while.
            let result = call_with::<CommandReceipt>(
                &["review", "send", &review_id, "--command-id", &id, "--no-wait"],
                Duration::from_secs(90),
            );
            (session_id, result)
        },
        ManagedAction::ProjectBoard(_)
        | ManagedAction::Inspect(_)
        | ManagedAction::InspectChecks(_)
        | ManagedAction::InspectReview(_)
        | ManagedAction::CancelCheck(_)
        | ManagedAction::EndTools { .. }
        | ManagedAction::Delegate { .. }
        | ManagedAction::InspectTask(_)
        | ManagedAction::Land { .. }
        | ManagedAction::Revise { .. } => unreachable!(),
    };
    let waiting = match result {
        Ok(receipt) => {
            update.notice = Some(receipt_notice(&receipt));
            (receipt.status == CommandStatus::Pending).then(|| (session_id.clone(), id.clone()))
        },
        Err(error) => {
            update.notice = Some(format!(" Result uncertain · query command {id} · {error}"));
            Some((session_id.clone(), id.clone()))
        },
    };
    if let Ok(detail) = call(&["session", "approvals", &session_id]) {
        update.detail = Some(detail);
    }
    (update, waiting)
}

fn receipt_notice(receipt: &CommandReceipt) -> String {
    format!(
        " {:?} · command {}{}",
        receipt.status,
        receipt.command_id,
        receipt.error.as_ref().map(|error| format!(" · {error}")).unwrap_or_default()
    )
}

fn acceptance(task_id: &str) -> Result<xaya_agent_proto::TaskAcceptance, String> {
    // The diff comes from Git; a large change takes a moment.
    call_with(&["task", "acceptance", task_id], Duration::from_secs(10))
}

type LatestReview =
    (Option<xaya_agent_proto::Review>, Option<String>, Vec<xaya_agent_proto::DependencySnapshot>);

/// The session's most recent review, if any, and why its task needs attention, if it does.
fn latest_review(session_id: &str) -> Result<LatestReview, String> {
    let review =
        call::<Vec<xaya_agent_proto::Review>>(&["review", "list", "--session", session_id])
            .map(|reviews| reviews.into_iter().next())?;
    let attention = match review.as_ref().and_then(|review| review.task_id.as_deref()) {
        Some(task) => call::<serde_json::Value>(&["task", "show", task])?["task"]["attention"]
            ["reason"]
            .as_str()
            .map(str::to_owned),
        None => None,
    };
    Ok((review, attention, call(&["task", "dependencies", "--session", session_id])?))
}

fn call<T: DeserializeOwned>(args: &[&str]) -> Result<T, String> {
    call_with(args, Duration::from_secs(3))
}

fn call_with<T: DeserializeOwned>(args: &[&str], timeout: Duration) -> Result<T, String> {
    call_in(args, timeout, None)
}

/// The user's login-shell PATH, read once. Opened from the Finder, XaYa inherits launchd's short
/// PATH, where the engines the user runs in a terminal are usually missing.
fn login_path() -> Option<&'static str> {
    static PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let shell = std::env::var_os("SHELL")?;
        // Markers keep whatever the startup files print out of the value.
        let script = r#"printf '\n__XAYA_PATH__%s__XAYA_PATH__\n' "$PATH""#;
        let mut child = Command::new(shell)
            .args(["-l", "-i", "-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            (&mut stdout).take(256 * 1024).read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().ok()?.is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
        parse_path(&String::from_utf8_lossy(&reader.join().ok()?.ok()?))
    })
    .as_deref()
}

fn parse_path(output: &str) -> Option<String> {
    let marker = "__XAYA_PATH__";
    let start = output.rfind(&format!("\n{marker}"))? + marker.len() + 1;
    let path = &output[start..];
    let path = &path[..path.find(marker)?];
    (!path.is_empty() && !path.contains('\n')).then(|| path.to_owned())
}

/// Run the service CLI, with `path` as its PATH when one is given.
fn call_in<T: DeserializeOwned>(
    args: &[&str],
    timeout: Duration,
    path: Option<&str>,
) -> Result<T, String> {
    let executable =
        std::env::current_exe().map_err(|error| error.to_string())?.with_file_name("xaya-agentd");
    let mut command = Command::new(executable);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("service stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("service stderr unavailable")?;
    let output_limit = match args {
        ["review", "list", ..] | ["task", "show" | "board" | "acceptance", ..] => 8 * 1024 * 1024,
        _ => 256 * 1024,
    };
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(output_limit).read_to_end(&mut bytes).map(|_| bytes)
    });
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.take(16 * 1024).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("service request timed out".to_owned());
            },
        }
    };
    let output =
        output.join().map_err(|_| "service reader failed")?.map_err(|error| error.to_string())?;
    let errors =
        errors.join().map_err(|_| "service reader failed")?.map_err(|error| error.to_string())?;
    if !status?.success() {
        return Err(String::from_utf8_lossy(&errors).chars().take(512).collect());
    }
    serde_json::from_slice(&output).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn running(token: &str) -> bool {
        Command::new("pgrep").args(["-f", token]).output().is_ok_and(|o| !o.stdout.is_empty())
    }

    fn until(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_login_path_is_read_between_its_markers() {
        let output = "Welcome!\n__XAYA_PATH__/opt/homebrew/bin:/usr/bin__XAYA_PATH__\nbye\n";
        assert_eq!(parse_path(output).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(parse_path("no markers"), None);
        assert_eq!(parse_path("\n__XAYA_PATH____XAYA_PATH__\n"), None);
    }

    #[test]
    fn board_watch_runs_only_while_refreshed_and_restarts_after_it_ends() {
        let token = format!("xaya-watch-test-{}-{:?}", std::process::id(), Instant::now());
        // Prints a board, then stays (`hold`) or exits at once, like a watch whose service ends.
        let hold = Arc::new(AtomicBool::new(true));
        let starts = Arc::new(AtomicUsize::new(0));
        let command = {
            let (token, hold, starts) = (token.clone(), Arc::clone(&hold), Arc::clone(&starts));
            move || {
                starts.fetch_add(1, Ordering::AcqRel);
                let script =
                    if hold.load(Ordering::Acquire) { "echo '[]'; sleep 10" } else { "echo '[]'" };
                let mut command = Command::new("/bin/sh");
                command.args(["-c", script, &token]);
                Some(command)
            }
        };
        let boards = Arc::new(AtomicUsize::new(0));
        let deliver = {
            let boards = Arc::clone(&boards);
            move |_rows: Vec<ManagedRow>| {
                boards.fetch_add(1, Ordering::AcqRel);
                true
            }
        };
        let watch = Arc::new(Watch::new(Duration::from_millis(400), Duration::from_millis(100)));
        let looping = {
            let watch = Arc::clone(&watch);
            thread::spawn(move || watch_loop(watch, command, deliver))
        };
        // Refreshed: one watch runs and its board arrives.
        let refresh = |watch: &Watch, for_: Duration| {
            let end = Instant::now() + for_;
            while Instant::now() < end {
                watch.touch();
                thread::sleep(Duration::from_millis(50));
            }
        };
        watch.touch();
        until("board delivered", || boards.load(Ordering::Acquire) >= 1);
        assert!(watch.live.load(Ordering::Acquire) && running(&token));
        refresh(&watch, Duration::from_millis(600));
        assert_eq!(starts.load(Ordering::Acquire), 1, "one watch while open");
        // No refreshes (panel closed): the child is ended and the board is no longer live.
        until("watch ended when idle", || !running(&token));
        until("not live", || !watch.live.load(Ordering::Acquire));
        // Reopened: a new watch starts.
        watch.touch();
        until("restarted", || starts.load(Ordering::Acquire) == 2 && running(&token));
        // A watch that ends by itself is started again while still wanted.
        hold.store(false, Ordering::Release);
        let _ = Command::new("pkill").args(["-f", &token]).status();
        refresh(&watch, Duration::from_millis(400));
        assert!(starts.load(Ordering::Acquire) >= 3, "{}", starts.load(Ordering::Acquire));
        // Closing ends the loop and leaves nothing running.
        hold.store(true, Ordering::Release);
        watch.closed.store(true, Ordering::Release);
        looping.join().unwrap();
        until("nothing left", || !running(&token));
    }
}
