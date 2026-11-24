//! What the programs in a window report through `OSC 7501`, as the window shows it: a title badge
//! and a desktop notification when a program starts waiting on the user, fails or finishes.
//!
//! With shell integration (`OSC 133`), commands that run long are reported the same way once
//! they finish.

use std::time::{Duration, Instant};

use alacritty_terminal::notification::Notification;
use alacritty_terminal::program_status::{
    InputKind, ProgramEvent, ProgramReport, ProgramState, ProgramStatus, ProgramTracker,
};
use alacritty_terminal::shell_integration::ShellMark;

/// Record id of the shell's last long command, beside the programs' own records.
const SHELL_RECORD: &str = "shell";

/// Program status of one window.
#[derive(Debug, Default)]
pub struct ProgramActivity {
    tracker: ProgramTracker,
    /// The summary currently shown, which notifications are decided against.
    shown: Option<ProgramStatus>,
    /// The terminal's foreground process group when running programs reported, see
    /// [`Self::foreground_changed`].
    foreground: Option<i32>,
    /// When the shown summary entered its state.
    since: Option<Instant>,
    /// When the shell's current command started (`OSC 133 ; C`).
    command_started: Option<Instant>,
    /// Whether a program reported its own status during the current command.
    command_reported: bool,
    /// The `app` of the program last reporting its status, until it's gone.
    app: Option<String>,
}

/// How much a window needs the user, for [`next_attention`]; smaller sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Attention {
    /// Waiting on the user, then failed, then finished.
    rank: u8,
    /// Within a rank, whoever has been waiting longest comes first.
    since: Instant,
}

/// The window to select after `current`, cycling through `windows` in attention order.
///
/// Starts at the most urgent window when `current` needs no attention itself, and is `None`
/// when no other window does.
pub fn next_attention<K: Copy + Eq>(
    current: K,
    windows: impl IntoIterator<Item = (K, Attention)>,
) -> Option<K> {
    let mut order: Vec<_> = windows.into_iter().collect();
    order.sort_by_key(|(_, attention)| *attention);
    let next = match order.iter().position(|(window, _)| *window == current) {
        Some(index) => order[(index + 1) % order.len()].0,
        None => order.first()?.0,
    };
    (next != current).then_some(next)
}

/// What changed after an update.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// The title badge changed.
    pub badge_changed: bool,
    /// The user should hear about it.
    pub notification: Option<Notification>,
}

impl ProgramActivity {
    /// Apply an event from the window's terminal.
    ///
    /// Results are acknowledged at once while the window is focused: the user is looking at them.
    pub fn handle(&mut self, event: ProgramEvent, focused: bool) -> Update {
        self.command_reported |= matches!(event, ProgramEvent::Report(_));
        match &event {
            ProgramEvent::Report(ProgramReport::Status(status)) if status.app.is_some() => {
                self.app.clone_from(&status.app);
            },
            // Agents clear their status when they exit.
            ProgramEvent::Report(ProgramReport::Clear(id)) if id.is_empty() => self.app = None,
            ProgramEvent::Report(_) => (),
            ProgramEvent::CommandStart | ProgramEvent::Reset => self.app = None,
        }
        let mut changed = self.tracker.apply(event);
        let notification = changed.then(|| self.transition()).flatten();
        if focused {
            changed |= self.tracker.acknowledge();
        }
        Update { badge_changed: changed && self.refresh(), notification }
    }

    /// Apply a shell integration mark.
    ///
    /// A command that ran for at least `threshold` reports its result like a program would: ✅ or
    /// ❌ and a notification. A command that reported its own status, such as an agent, already
    /// told the user, and `None` ignores commands altogether.
    pub fn shell_mark(
        &mut self,
        mark: ShellMark,
        focused: bool,
        threshold: Option<Duration>,
        now: Instant,
    ) -> Update {
        match mark {
            ShellMark::PromptStart => {
                self.command_started = None;
                self.handle(ProgramEvent::CommandStart, focused)
            },
            ShellMark::InputStart => Update::default(),
            ShellMark::CommandStart => {
                self.command_started = Some(now);
                self.command_reported = false;
                Update::default()
            },
            ShellMark::CommandEnd(code) => {
                let Some(started) = self.command_started.take() else {
                    return Update::default();
                };
                let elapsed = now.saturating_duration_since(started);
                if self.command_reported || threshold.is_none_or(|threshold| elapsed < threshold) {
                    return Update::default();
                }

                let failed = code.is_some_and(|code| code != 0);
                let status = ProgramStatus {
                    id: SHELL_RECORD.into(),
                    state: if failed { ProgramState::Error } else { ProgramState::Done },
                    kind: None,
                    app: None,
                    title: None,
                    message: None,
                    progress: None,
                };
                let mut changed =
                    self.tracker.apply(ProgramEvent::Report(ProgramReport::Status(status)));
                if focused {
                    changed |= self.tracker.acknowledge();
                }

                let headline = match code {
                    Some(code) if code != 0 => format!("Command failed with exit code {code}"),
                    _ => "Command finished".into(),
                };
                let title = format!("{headline} after {}", format_duration(elapsed));
                let notification = Notification { title: Some(title), body: String::new() };
                Update {
                    badge_changed: changed && self.refresh(),
                    notification: Some(notification),
                }
            },
        }
    }

    /// The window gained focus, so its results have been seen.
    pub fn focus(&mut self) -> bool {
        self.tracker.acknowledge() && self.refresh()
    }

    /// Whether a program reported it's still running, while which the foreground is watched.
    pub fn is_running(&self) -> bool {
        self.tracker.has_running()
    }

    /// Whether a program is in the middle of a turn, whose changes to the checkout are tracked,
    /// see [`crate::turn_changes`].
    pub fn in_turn(&self) -> bool {
        self.tracker.is_busy()
    }

    /// Remember `pgrp` as the foreground process group the running programs belong to.
    pub fn note_foreground(&mut self, pgrp: i32) {
        if self.tracker.has_running() {
            self.foreground.get_or_insert(pgrp);
        } else {
            self.foreground = None;
        }
    }

    /// The terminal's foreground process group is now `pgrp`, returning whether the badge
    /// changed.
    ///
    /// Without shell integration nothing tells the terminal a program ended. One killed mid-turn
    /// never reports again, so a different foreground job (usually the shell, back at its prompt)
    /// ends what was still running, like a new prompt would. Results stay.
    pub fn foreground_changed(&mut self, pgrp: i32) -> bool {
        match self.foreground {
            Some(noted) if noted != pgrp => {
                self.foreground = None;
                self.app = None;
                self.tracker.end_command() && self.refresh()
            },
            _ => false,
        }
    }

    /// The shell exited, taking every program with it.
    pub fn child_exit(&mut self) -> bool {
        self.app = None;
        self.tracker.reset() && self.refresh()
    }

    /// The `app` of the program running in the window, such as `codex`, as long as it reported
    /// its status since it started.
    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    /// Whether `OSC 9` / `OSC 777` notifications from this window should be dropped.
    ///
    /// A program that reports its status announces the same moments through them too, and less
    /// precisely (Claude Code says "needs your permission" when it asks a question), so the
    /// status reports win while any are kept.
    pub fn supersedes_notifications(&self) -> bool {
        !self.tracker.is_empty()
    }

    /// Whether a program waits on the user or failed unseen, which the Dock badge counts.
    pub fn needs_attention(&self) -> bool {
        self.shown.as_ref().is_some_and(|status| {
            matches!(status.state, ProgramState::Blocked | ProgramState::Error)
        })
    }

    /// How much this window needs the user, `None` when it doesn't.
    pub fn attention(&self) -> Option<Attention> {
        let rank = match self.shown.as_ref()?.state {
            ProgramState::Blocked => 0,
            ProgramState::Error => 1,
            ProgramState::Done => 2,
            ProgramState::Working | ProgramState::Idle => return None,
        };
        Some(Attention { rank, since: self.since? })
    }

    /// The status this window shows, and when it entered its state.
    pub fn shown(&self) -> Option<(&ProgramStatus, Instant)> {
        Some((self.shown.as_ref()?, self.since?))
    }

    /// How urgently the shown status needs the user, lower first; `None` without one.
    pub fn urgency(&self) -> Option<u8> {
        Some(match self.shown.as_ref()?.state {
            ProgramState::Blocked => 0,
            ProgramState::Error => 1,
            ProgramState::Working => 2,
            ProgramState::Done => 3,
            ProgramState::Idle => 4,
        })
    }

    /// Prefix for the window title, such as `⏳ 25%`.
    pub fn badge(&self) -> Option<String> {
        let status = self.shown.as_ref()?;
        let badge = match status.state {
            ProgramState::Working => match status.progress {
                Some(progress) => return Some(format!("⏳ {progress}%")),
                None => "⏳",
            },
            ProgramState::Blocked => "🔔",
            ProgramState::Done => "✅",
            ProgramState::Error => "❌",
            ProgramState::Idle => return None,
        };
        Some(badge.into())
    }

    /// The notification for the summary's change since it was last shown, if any.
    ///
    /// Only entering a state rings, so repeated reports and a working program's progress stay
    /// quiet. A finish rings only after work was seen, since a program that merely reports its
    /// previous result on start finished nothing now.
    fn transition(&self) -> Option<Notification> {
        let current = self.tracker.summary()?;
        let previous = self.shown.as_ref().map(|status| status.state);
        let headline = match current.state {
            ProgramState::Blocked if previous != Some(ProgramState::Blocked) => {
                match current.kind {
                    Some(InputKind::Permission) => "Needs your permission",
                    Some(InputKind::Question) => "Has a question for you",
                    Some(InputKind::Auth) => "Needs you to sign in",
                    None => "Waiting for your input",
                }
            },
            ProgramState::Error if previous != Some(ProgramState::Error) => "Failed",
            ProgramState::Done
                if matches!(previous, Some(ProgramState::Working | ProgramState::Blocked)) =>
            {
                "Finished"
            },
            _ => return None,
        };

        let app = current.app.as_deref().map_or("XaYa", display_name);
        let body = current.message.or(current.title).unwrap_or_default();
        Some(Notification { title: Some(format!("{app}: {headline}")), body })
    }

    /// Recompute the shown summary, returning whether the badge changed.
    fn refresh(&mut self) -> bool {
        let before = self.badge();
        let summary = self.tracker.summary();
        let state = |status: &Option<ProgramStatus>| status.as_ref().map(|status| status.state);
        if state(&summary) != state(&self.shown) {
            self.since = Some(Instant::now());
        }
        self.shown = summary;
        self.badge() != before
    }
}

/// `elapsed` as `42s`, `3m 07s` or `1h 05m`.
pub fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}

/// Readable name of a reported `app`.
pub fn display_name(app: &str) -> &str {
    match app {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        app => app,
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::program_status::parse_report;

    use super::*;

    fn report(activity: &mut ProgramActivity, body: &str, focused: bool) -> Update {
        activity.handle(ProgramEvent::Report(parse_report(body).unwrap()), focused)
    }

    fn headline(update: &Update) -> Option<&str> {
        update.notification.as_ref().and_then(|n| n.title.as_deref())
    }

    #[test]
    fn a_claude_turn_badges_and_rings_once() {
        let mut activity = ProgramActivity::default();
        let idle = report(&mut activity, "state=idle:app=claude-code", false);
        assert_eq!(idle, Update::default());

        let working = report(&mut activity, "state=working:app=claude-code", false);
        assert!(working.badge_changed && working.notification.is_none());
        assert_eq!(activity.badge().as_deref(), Some("⏳"));

        let done = report(&mut activity, "state=done:app=claude-code", false);
        assert_eq!(headline(&done), Some("Claude Code: Finished"));
        assert_eq!(activity.badge().as_deref(), Some("✅"));

        // Repeating it changes nothing.
        assert_eq!(report(&mut activity, "state=done:app=claude-code", false), Update::default());

        // Looking at the window clears the result.
        assert!(activity.focus());
        assert_eq!(activity.badge(), None);
        assert!(!activity.supersedes_notifications());
    }

    #[test]
    fn questions_carry_the_programs_message() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=claude-code", false);
        let update = report(
            &mut activity,
            "state=blocked:app=claude-code:kind=question:msg=d2hpY2ggY29sb3I/",
            false,
        );
        let notification = update.notification.unwrap();
        assert_eq!(notification.title.as_deref(), Some("Claude Code: Has a question for you"));
        assert_eq!(notification.body, "which color?");
        assert_eq!(activity.badge().as_deref(), Some("🔔"));
        assert!(activity.supersedes_notifications());
    }

    #[test]
    fn finishing_without_work_and_progress_stay_quiet() {
        let mut activity = ProgramActivity::default();
        assert_eq!(headline(&report(&mut activity, "state=done:app=codex", false)), None);

        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", false);
        let progress = report(&mut activity, "state=working:app=codex:progress=40", false);
        assert!(progress.badge_changed && progress.notification.is_none());
        assert_eq!(activity.badge().as_deref(), Some("⏳ 40%"));
    }

    #[test]
    fn focused_windows_ring_but_keep_no_result_badge() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", true);
        // Whether to show it while focused is the notification config's call.
        let done = report(&mut activity, "state=done:app=codex", true);
        assert_eq!(headline(&done), Some("Codex: Finished"));
        assert_eq!(activity.badge(), None);
    }

    #[test]
    fn prompts_and_exits_end_live_status() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", false);
        let update = activity.handle(ProgramEvent::CommandStart, false);
        assert!(update.badge_changed && update.notification.is_none());
        assert_eq!(activity.badge(), None);

        report(&mut activity, "state=error:app=codex", false);
        assert!(activity.child_exit());
        assert!(!activity.supersedes_notifications());
    }

    #[test]
    fn a_new_foreground_job_ends_a_killed_program() {
        let (shell, claude) = (100, 200);
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=claude-code", false);
        assert!(activity.is_running());
        activity.note_foreground(claude);

        // Still the same job: nothing happens.
        assert!(!activity.foreground_changed(claude));
        activity.note_foreground(claude);
        assert_eq!(activity.badge().as_deref(), Some("⏳"));

        // Ctrl-C: the shell is back in the foreground.
        assert!(activity.foreground_changed(shell));
        assert_eq!(activity.badge(), None);
        assert!(!activity.is_running());
    }

    #[test]
    fn foreground_changes_keep_results() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:id=a:app=codex", false);
        report(&mut activity, "state=done:id=b:app=codex", false);
        activity.note_foreground(200);
        assert!(activity.foreground_changed(100));
        assert_eq!(activity.badge().as_deref(), Some("✅"));
        // Nothing is running any more, so nothing is watched.
        activity.note_foreground(100);
        assert!(!activity.foreground_changed(300));
    }

    #[test]
    fn waiting_and_failures_need_attention() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", false);
        assert!(!activity.needs_attention());
        report(&mut activity, "state=blocked:app=codex:kind=permission", false);
        assert!(activity.needs_attention());
        report(&mut activity, "state=done:app=codex", false);
        assert!(!activity.needs_attention());
        report(&mut activity, "state=error:app=codex", false);
        assert!(activity.needs_attention());
        activity.focus();
        assert!(!activity.needs_attention());
    }

    #[test]
    fn attention_ranks_waiting_over_failed_over_finished() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", false);
        assert_eq!(activity.attention(), None);
        report(&mut activity, "state=done:app=codex", false);
        let done = activity.attention().unwrap();
        report(&mut activity, "state=error:app=codex", false);
        let error = activity.attention().unwrap();
        report(&mut activity, "state=blocked:app=codex", false);
        let blocked = activity.attention().unwrap();
        assert!(blocked < error && error < done);
    }

    #[test]
    fn next_attention_cycles_in_order() {
        let now = Instant::now();
        let at = |rank, seconds| Attention {
            rank,
            since: now + std::time::Duration::from_secs(seconds),
        };
        // 'b' waits longest among the blocked; 'd' only finished.
        let windows = [('a', at(0, 5)), ('b', at(0, 1)), ('c', at(1, 0)), ('d', at(2, 0))];

        // From a window that needs nothing, go to the most urgent one.
        assert_eq!(next_attention('x', windows), Some('b'));
        // Then cycle through the rest and back around.
        assert_eq!(next_attention('b', windows), Some('a'));
        assert_eq!(next_attention('a', windows), Some('c'));
        assert_eq!(next_attention('c', windows), Some('d'));
        assert_eq!(next_attention('d', windows), Some('b'));

        // Nowhere else to go.
        assert_eq!(next_attention('a', [('a', at(0, 0))]), None);
        assert_eq!(next_attention('x', []), None);
    }

    fn mark(activity: &mut ProgramActivity, mark: ShellMark, at: Instant) -> Update {
        activity.shell_mark(mark, false, Some(Duration::from_secs(10)), at)
    }

    #[test]
    fn long_commands_report_when_they_finish() {
        let start = Instant::now();
        let mut activity = ProgramActivity::default();
        mark(&mut activity, ShellMark::CommandStart, start);
        let end =
            mark(&mut activity, ShellMark::CommandEnd(Some(0)), start + Duration::from_secs(83));
        assert_eq!(headline(&end), Some("Command finished after 1m 23s"));
        assert_eq!(activity.badge().as_deref(), Some("✅"));

        // The next prompt keeps the result until the window is seen.
        mark(&mut activity, ShellMark::PromptStart, start);
        assert_eq!(activity.badge().as_deref(), Some("✅"));

        mark(&mut activity, ShellMark::CommandStart, start);
        let failed =
            mark(&mut activity, ShellMark::CommandEnd(Some(2)), start + Duration::from_secs(12));
        assert_eq!(headline(&failed), Some("Command failed with exit code 2 after 12s"));
        assert_eq!(activity.badge().as_deref(), Some("❌"));
        assert!(activity.focus());
        assert_eq!(activity.badge(), None);
    }

    #[test]
    fn short_reported_and_unstarted_commands_stay_quiet() {
        let start = Instant::now();
        let mut activity = ProgramActivity::default();
        mark(&mut activity, ShellMark::CommandStart, start);
        let quick =
            mark(&mut activity, ShellMark::CommandEnd(Some(1)), start + Duration::from_secs(9));
        assert_eq!(quick, Update::default());

        // An agent reported its own status during the command.
        mark(&mut activity, ShellMark::CommandStart, start);
        report(&mut activity, "state=working:app=claude-code", false);
        let agent =
            mark(&mut activity, ShellMark::CommandEnd(Some(0)), start + Duration::from_secs(600));
        assert_eq!(agent.notification, None);

        // A finish without a start, and commands while notifications for them are off.
        assert_eq!(mark(&mut activity, ShellMark::CommandEnd(Some(0)), start), Update::default());
        activity.shell_mark(ShellMark::CommandStart, false, None, start);
        let off = activity.shell_mark(
            ShellMark::CommandEnd(Some(0)),
            false,
            None,
            start + Duration::from_secs(600),
        );
        assert_eq!(off, Update::default());
    }

    #[test]
    fn a_new_prompt_ends_running_programs() {
        let start = Instant::now();
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", false);
        let prompt = mark(&mut activity, ShellMark::PromptStart, start);
        assert!(prompt.badge_changed);
        assert_eq!(activity.badge(), None);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(187)), "3m 07s");
        assert_eq!(format_duration(Duration::from_secs(3900)), "1h 05m");
    }

    #[test]
    fn the_reporting_app_is_kept_until_it_is_gone() {
        let mut activity = ProgramActivity::default();
        report(&mut activity, "state=working:app=codex", true);
        report(&mut activity, "state=done", true);
        // Seen results are gone, but Codex still runs.
        assert_eq!(activity.badge(), None);
        assert_eq!(activity.app(), Some("codex"));

        report(&mut activity, "state=clear", false);
        assert_eq!(activity.app(), None);

        report(&mut activity, "state=idle:app=claude-code", false);
        activity.handle(ProgramEvent::CommandStart, false);
        assert_eq!(activity.app(), None);
    }
}
