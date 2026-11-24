//! What the programs in each window report, as `msg status` and the agent panel list it.

use std::cmp::Reverse;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use alacritty_terminal::program_status::{InputKind, ProgramState};

use crate::program_status::{ProgramActivity, display_name, format_duration};

/// What the programs in one window report, for `msg status`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WindowStatus {
    pub window_id: u64,
    /// Window title, without the status badge.
    pub title: String,
    pub focused: bool,
    /// `working`, `blocked`, `done`, `error` or `idle`; absent when nothing reported.
    pub state: Option<String>,
    /// What a blocked program waits for: `permission`, `question` or `auth`.
    pub kind: Option<String>,
    /// Reporting program, such as `claude-code` or `codex`.
    pub app: Option<String>,
    pub message: Option<String>,
    /// Percentage, while working.
    pub progress: Option<u8>,
    /// Seconds since the window entered its state.
    pub seconds: Option<u64>,
    /// What its agent's last turn changed in the checkout, when it changed anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Box<TurnChanges>>,
}

/// What an agent's turn changed in its checkout, between two snapshots of the working tree.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TurnChanges {
    pub files: usize,
    pub insertions: u64,
    pub deletions: u64,
    /// The checkout's top-level directory.
    pub root: PathBuf,
    /// Trees of the working tree before and after the turn, for `git diff`.
    pub before: String,
    pub after: String,
}

impl TurnChanges {
    /// Such as `3 files +48 -3`.
    pub fn label(&self) -> String {
        let files = if self.files == 1 { "file" } else { "files" };
        format!("{} {files} +{} -{}", self.files, self.insertions, self.deletions)
    }
}

impl WindowStatus {
    pub fn new(
        window_id: u64,
        title: String,
        focused: bool,
        activity: &ProgramActivity,
        now: Instant,
    ) -> Self {
        let shown = activity.shown();
        let status = shown.map(|(status, _)| status);
        Self {
            window_id,
            title,
            focused,
            state: status.map(|status| state_name(status.state).into()),
            kind: status.and_then(|status| status.kind).map(|kind| kind_name(kind).into()),
            app: status.and_then(|status| status.app.clone()),
            message: status.and_then(|status| status.message.clone().or(status.title.clone())),
            progress: status.and_then(|status| status.progress),
            seconds: shown.map(|(_, since)| now.saturating_duration_since(since).as_secs()),
            changes: None,
        }
    }

    /// Whether a program in this window is still working.
    pub fn is_working(&self) -> bool {
        self.state.as_deref() == Some("working")
    }

    /// Sort key: what needs the user first, and within a state whoever has waited longest.
    fn order(&self) -> (u8, Reverse<u64>, u64) {
        let rank = match self.state.as_deref() {
            Some("blocked") => 0,
            Some("error") => 1,
            Some("done") => 2,
            Some("working") => 3,
            Some(_) => 4,
            None => 5,
        };
        (rank, Reverse(self.seconds.unwrap_or_default()), self.window_id)
    }

    /// The state as shown in the table, such as `waiting: permission` or `working 40%`.
    pub fn state_label(&self) -> String {
        match (self.state.as_deref(), &self.kind, self.progress) {
            (Some("blocked"), Some(kind), _) => format!("waiting: {kind}"),
            (Some("blocked"), None, _) => "waiting".into(),
            (Some("working"), _, Some(progress)) => format!("working {progress}%"),
            (Some("error"), ..) => "failed".into(),
            (Some(state), ..) => state.into(),
            (None, ..) => "-".into(),
        }
    }
}

/// Order windows the way `SelectNextAttention` visits them, then the rest.
pub fn sort_statuses(windows: &mut [WindowStatus]) {
    windows.sort_by_key(WindowStatus::order);
}

/// A window's status changing, for `msg status --watch`.
#[cfg(unix)]
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StatusChange {
    /// The window appeared or its program reported something new.
    Changed(WindowStatus),
    /// The window closed.
    Closed { window_id: u64 },
}

#[cfg(unix)]
impl StatusChange {
    /// One line for a terminal, such as `42  waiting: permission  Claude Code  web  — approve?`.
    pub fn format(&self) -> String {
        match self {
            Self::Changed(window) => {
                let app = window.app.as_deref().map_or("-", display_name);
                let mut line = format!(
                    "{:<20} {:<20} {:<12} {}",
                    window.window_id,
                    window.state_label(),
                    app,
                    window.title
                );
                if let Some(message) = &window.message {
                    let message: String = message.chars().take(100).collect();
                    line.push_str(&format!("  — {message}"));
                }
                line
            },
            Self::Closed { window_id } => format!("{window_id:<20} closed"),
        }
    }
}

/// What changed from `before` to `after`.
///
/// Only what programs report counts: durations tick, focus moves and agents animate their titles
/// (Codex spins one every frame), none of which is news.
#[cfg(unix)]
pub fn changes(before: &[WindowStatus], after: &[WindowStatus]) -> Vec<StatusChange> {
    let report = |window: &WindowStatus| {
        (
            window.state.clone(),
            window.kind.clone(),
            window.app.clone(),
            window.message.clone(),
            window.progress,
            window.changes.clone(),
        )
    };
    let mut changes: Vec<_> = after
        .iter()
        .filter(|window| {
            let previous = before.iter().find(|previous| previous.window_id == window.window_id);
            previous.is_none_or(|previous| report(previous) != report(window))
        })
        .cloned()
        .map(StatusChange::Changed)
        .collect();
    changes.extend(
        before
            .iter()
            .filter(|window| after.iter().all(|now| now.window_id != window.window_id))
            .map(|window| StatusChange::Closed { window_id: window.window_id }),
    );
    changes
}

/// Human-readable table of `windows`; `*` marks the focused one.
pub fn format_statuses(windows: &[WindowStatus]) -> String {
    let mut table =
        format!("  {:<20} {:<20} {:<12} {:<8} {}\n", "WINDOW", "STATE", "APP", "FOR", "TITLE");
    for window in windows {
        let focus = if window.focused { '*' } else { ' ' };
        let app = window.app.as_deref().map_or("-", display_name);
        let elapsed = window.seconds.map(|seconds| format_duration(Duration::from_secs(seconds)));
        let mut line = format!(
            "{focus} {:<20} {:<20} {:<12} {:<8} {}",
            window.window_id,
            window.state_label(),
            app,
            elapsed.as_deref().unwrap_or("-"),
            window.title,
        );
        if let Some(message) = &window.message {
            let message: String = message.chars().take(100).collect();
            line.push_str(&format!("\n  {:<20} {message}", ""));
        }
        table.push_str(line.trim_end());
        table.push('\n');
    }
    table
}

fn state_name(state: ProgramState) -> &'static str {
    match state {
        ProgramState::Idle => "idle",
        ProgramState::Working => "working",
        ProgramState::Blocked => "blocked",
        ProgramState::Done => "done",
        ProgramState::Error => "error",
    }
}

fn kind_name(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Permission => "permission",
        InputKind::Question => "question",
        InputKind::Auth => "auth",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::program_status::{ProgramEvent, parse_report};

    fn activity(reports: &[&str]) -> ProgramActivity {
        let mut activity = ProgramActivity::default();
        for body in reports {
            activity.handle(ProgramEvent::Report(parse_report(body).unwrap()), false);
        }
        activity
    }

    /// Window `id` after `reports`, looked at `elapsed` later; window 1 has focus.
    fn window(id: u64, reports: &[&str], elapsed: Duration) -> WindowStatus {
        let activity = activity(reports);
        let now = Instant::now() + elapsed;
        WindowStatus::new(id, format!("window {id}"), id == 1, &activity, now)
    }

    #[test]
    fn status_describes_what_the_window_shows() {
        let now = Duration::from_secs(5);
        // "approve?" in base64.
        let blocked = window(
            1,
            &["state=working:app=claude-code", "state=blocked:kind=permission:msg=YXBwcm92ZT8="],
            now,
        );
        assert_eq!(blocked.state.as_deref(), Some("blocked"));
        assert_eq!(blocked.kind.as_deref(), Some("permission"));
        assert_eq!(blocked.message.as_deref(), Some("approve?"));
        assert!(blocked.focused && !blocked.is_working());
        assert!(blocked.seconds.unwrap() >= 5);
        // The later report replaced the earlier one, app included.
        assert_eq!(blocked.app, None);

        let working = window(2, &["state=working:app=codex:progress=40"], now);
        assert!(working.is_working());
        assert_eq!(working.progress, Some(40));
        assert_eq!(working.state_label(), "working 40%");

        let quiet = window(3, &[], now);
        assert_eq!(quiet.state, None);
        assert_eq!(quiet.seconds, None);
        assert_eq!(quiet.state_label(), "-");
    }

    #[test]
    fn statuses_sort_by_what_needs_the_user() {
        let now = Duration::ZERO;
        let mut windows = vec![
            window(1, &[], now),
            window(2, &["state=working:app=codex"], now),
            window(3, &["state=working:app=codex", "state=done:app=codex"], now),
            window(4, &["state=error:app=codex"], now),
            window(5, &["state=blocked:app=codex"], now),
        ];
        // Of two windows waiting on the user, the one waiting longer comes first.
        let mut older = window(6, &["state=blocked:app=codex"], now);
        older.seconds = Some(600);
        windows.push(older);

        sort_statuses(&mut windows);
        let order: Vec<_> = windows.iter().map(|window| window.window_id).collect();
        assert_eq!(order, [6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn status_table_is_readable() {
        let now = Duration::from_secs(83);
        let windows = [
            window(
                1,
                &[
                    "state=working:app=claude-code",
                    "state=blocked:app=claude-code:kind=question:msg=d2hpY2ggY29sb3I/",
                ],
                now,
            ),
            window(22, &[], now),
        ];
        let table = format_statuses(&windows);
        let lines: Vec<_> = table.lines().collect();
        assert!(lines[0].starts_with("  WINDOW"));
        assert!(lines[1].starts_with("* 1 "), "{table}");
        assert!(lines[1].contains("waiting: question"));
        assert!(lines[1].contains("Claude Code"));
        assert!(lines[1].contains("1m 23s"));
        assert!(lines[1].ends_with("window 1"));
        assert_eq!(lines[2].trim(), "which color?");
        assert!(lines[3].starts_with("  22 "));
        assert!(lines[3].ends_with("window 22"));
    }

    #[test]
    #[cfg(unix)]
    fn watching_reports_what_programs_change() {
        let now = Duration::ZERO;
        let before = vec![window(1, &[], now), window(2, &["state=working:app=codex"], now)];
        assert_eq!(changes(&[], &before).len(), 2);
        assert!(changes(&before, &before).is_empty());

        // Ticking durations, moving focus and spinning titles aren't changes.
        let mut quiet = before.clone();
        quiet[1].seconds = Some(30);
        quiet[1].focused = true;
        quiet[1].title = "⠸ api".into();
        assert!(changes(&before, &quiet).is_empty());

        let after = vec![window(2, &["state=working:app=codex", "state=done:app=codex"], now)];
        let changes = changes(&before, &after);
        assert_eq!(changes, [StatusChange::Changed(after[0].clone()), StatusChange::Closed {
            window_id: 1
        }]);
        assert!(changes[0].format().starts_with("2 "));
        assert!(changes[0].format().contains("done"));
        assert_eq!(changes[1].format().split_whitespace().collect::<Vec<_>>(), ["1", "closed"]);

        // Events are tagged JSON objects.
        let json = serde_json::to_value(&changes[1]).unwrap();
        assert_eq!(json, serde_json::json!({ "event": "closed", "window_id": 1 }));
        let json = serde_json::to_value(&changes[0]).unwrap();
        assert_eq!(json["event"], "changed");
        assert_eq!(json["state"], "done");
    }
}
