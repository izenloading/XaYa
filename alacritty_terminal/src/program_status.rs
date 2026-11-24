//! The Program Status Protocol (`OSC 7501`), revision 0.4.
//!
//! Programs such as Claude Code and Codex report whether they're working, waiting on the user or
//! done, so the terminal can show it outside the grid. A program asks `OSC 7501 ; ? ST` first and
//! only reports once the terminal answered with [`SUPPORT_REPLY`].
//!
//! A report is `OSC 7501 ; key=value:key=value… ST`. Free text (`msg`, `title`) is base64 encoded.
//! Reports are validated whole: one malformed or oversized field discards the report, so a
//! program can't half-apply one.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

/// Capabilities announced in reply to an `OSC 7501 ; ?` probe.
pub const SUPPORT_REPLY: &str =
    "\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// Most records kept per terminal; the least recently reported one makes room.
const MAX_RECORDS: usize = 256;

/// Longest raw `msg` and its decoded length, in bytes.
const MAX_MESSAGE: (usize, usize) = (2732, 2048);

/// Longest raw `title` and its decoded length, in bytes.
const MAX_TITLE: (usize, usize) = (256, 192);

/// Longest `app`, in bytes.
const MAX_APP: usize = 32;

/// What a program is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    /// Waiting on the user, see [`InputKind`].
    Blocked,
    Done,
    Error,
}

impl ProgramState {
    /// How urgently this state needs the user; lower is more urgent.
    fn attention_rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Error => 1,
            Self::Working => 2,
            Self::Done => 3,
            Self::Idle => 4,
        }
    }

    /// Whether this is a result that stays until the user saw it.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Error)
    }
}

/// What a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Permission,
    Question,
    Auth,
}

/// One program's reported status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    /// Record id; `/` separates a child (e.g. a subagent) from its parent. Empty for the root.
    pub id: String,
    pub state: ProgramState,
    /// Only set while [`ProgramState::Blocked`].
    pub kind: Option<InputKind>,
    /// Short program identifier, such as `claude-code` or `codex`.
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    /// Percentage, `0..=100`.
    pub progress: Option<u8>,
}

/// A parsed `OSC 7501` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramReport {
    Status(ProgramStatus),
    /// Remove the record with this id and its children; an empty id removes all of them.
    Clear(String),
}

/// What the PTY stream tells the status tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramEvent {
    Report(ProgramReport),
    /// The shell drew a new prompt (`OSC 133 ; A`), so the foreground program is gone.
    CommandStart,
    /// `RIS`: the terminal was fully reset.
    Reset,
}

/// Parse a report body, the part after `7501;`.
pub fn parse_report(body: &str) -> Option<ProgramReport> {
    let mut pairs = BTreeMap::new();
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key.len() > 16 {
            return None;
        }

        // Limits apply to every pair, including overwritten duplicates. An oversized field
        // discards the report rather than silently disappearing from it.
        let too_long = match key {
            "msg" => value.len() > MAX_MESSAGE.0,
            "title" => value.len() > MAX_TITLE.0,
            "app" => value.len() > MAX_APP,
            "id" => !valid_id(value),
            _ => false,
        };
        if too_long {
            return None;
        }

        let valid_key = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase());
        let valid_value =
            value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b));
        if !valid_key || !valid_value {
            continue;
        }
        pairs.insert(key, value);
    }

    let id = pairs.get("id").map_or_else(String::new, |id| (*id).to_owned());
    let state = match *pairs.get("state")? {
        "idle" => ProgramState::Idle,
        "working" => ProgramState::Working,
        "blocked" => ProgramState::Blocked,
        "done" => ProgramState::Done,
        "error" => ProgramState::Error,
        "clear" => return Some(ProgramReport::Clear(id)),
        _ => return None,
    };

    let kind = match (state, pairs.get("kind").copied()) {
        (ProgramState::Blocked, Some("permission")) => Some(InputKind::Permission),
        (ProgramState::Blocked, Some("question")) => Some(InputKind::Question),
        (ProgramState::Blocked, Some("auth")) => Some(InputKind::Auth),
        _ => None,
    };

    Some(ProgramReport::Status(ProgramStatus {
        id,
        state,
        kind,
        app: pairs.get("app").map(|app| (*app).to_owned()),
        title: text(pairs.get("title").copied(), MAX_TITLE.1)?,
        message: text(pairs.get("msg").copied(), MAX_MESSAGE.1)?,
        progress: pairs.get("progress").and_then(|p| p.parse().ok()).filter(|p| *p <= 100),
    }))
}

/// Whether `id` is up to 8 `/`-separated segments of `[A-Za-z0-9_.+-]{1,32}`, or empty.
fn valid_id(id: &str) -> bool {
    id.is_empty()
        || (id.len() <= 128
            && id.split('/').count() <= 8
            && id.split('/').all(|segment| {
                !segment.is_empty()
                    && segment.len() <= 32
                    && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
            }))
}

/// Decode a base64 text field.
///
/// `Some(None)` when absent, `None` when present but invalid, which discards the report.
fn text(value: Option<&str>, max_decoded: usize) -> Option<Option<String>> {
    let Some(value) = value else { return Some(None) };
    let bytes = STANDARD.decode(value).or_else(|_| STANDARD_NO_PAD.decode(value)).ok()?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    // These strings are shown outside the grid, where bidi overrides could disguise them.
    let text: String = text.chars().filter(|&c| !crate::notification::is_bidi_control(c)).collect();
    let text = text.trim();
    Some((!text.is_empty()).then(|| text.to_owned()))
}

/// Answers allowed per [`PROBE_WINDOW`].
const MAX_PROBE_REPLIES: u32 = 8;

/// Window over which [`MAX_PROBE_REPLIES`] applies.
const PROBE_WINDOW: Duration = Duration::from_secs(10);

/// Bounds how often probes are answered.
///
/// Answers go to the program's input, which a program that never reads it would fill up.
#[derive(Debug, Default)]
pub struct ProbeLimiter {
    window_start: Option<Instant>,
    replies: u32,
}

impl ProbeLimiter {
    /// Whether a probe arriving at `now` may be answered, counting it if so.
    pub fn allow(&mut self, now: Instant) -> bool {
        if self.window_start.is_none_or(|start| now.duration_since(start) >= PROBE_WINDOW) {
            self.window_start = Some(now);
            self.replies = 0;
        }
        self.replies += 1;
        self.replies <= MAX_PROBE_REPLIES
    }
}

/// The status records of one terminal.
#[derive(Debug, Default)]
pub struct ProgramTracker {
    records: BTreeMap<String, Record>,
    clock: u64,
}

#[derive(Debug)]
struct Record {
    status: ProgramStatus,
    /// When the record was last reported, in [`ProgramTracker::clock`] ticks.
    order: u64,
}

impl ProgramTracker {
    /// Apply an event from the PTY stream, returning whether any record changed.
    pub fn apply(&mut self, event: ProgramEvent) -> bool {
        match event {
            ProgramEvent::Report(ProgramReport::Status(status)) => self.report(status),
            ProgramEvent::Report(ProgramReport::Clear(id)) => self.clear(&id),
            ProgramEvent::CommandStart => self.end_command(),
            ProgramEvent::Reset => self.reset(),
        }
    }

    /// Drop records that are still in progress, keeping results the user hasn't seen.
    ///
    /// The program behind them is gone, so they'd never be updated again; its results are never
    /// invented from its exit.
    pub fn end_command(&mut self) -> bool {
        self.retain(|status| status.state.is_settled())
    }

    /// Drop results once the user has seen them.
    pub fn acknowledge(&mut self) -> bool {
        self.retain(|status| !status.state.is_settled())
    }

    /// Drop every record.
    pub fn reset(&mut self) -> bool {
        self.retain(|_| false)
    }

    /// Whether any program reported a status that is still kept.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether any record is still in progress, rather than a result.
    pub fn has_running(&self) -> bool {
        self.records.values().any(|record| !record.status.state.is_settled())
    }

    /// Whether a program is in the middle of a turn: working, or waiting on the user mid-way.
    ///
    /// Unlike [`Self::has_running`], an idle program at its prompt doesn't count.
    pub fn is_busy(&self) -> bool {
        self.records.values().any(|record| {
            matches!(record.status.state, ProgramState::Working | ProgramState::Blocked)
        })
    }

    /// The record most in need of the user: blocked over failed over working over done, then the
    /// most recently reported. A child without an `app` reports as its nearest ancestor's.
    pub fn summary(&self) -> Option<ProgramStatus> {
        let record = self.records.values().min_by(|a, b| {
            let rank = |record: &Record| record.status.state.attention_rank();
            rank(a).cmp(&rank(b)).then(b.order.cmp(&a.order))
        })?;

        let mut status = record.status.clone();
        let mut ancestor = status.id.as_str();
        while status.app.is_none() && !ancestor.is_empty() {
            ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
            status.app = self.records.get(ancestor).and_then(|r| r.status.app.clone());
        }
        Some(status)
    }

    fn report(&mut self, status: ProgramStatus) -> bool {
        self.clock += 1;
        if let Some(record) = self.records.get_mut(&status.id) {
            record.order = self.clock;
            if record.status == status {
                return false;
            }
            record.status = status;
            return true;
        }

        if self.records.len() == MAX_RECORDS {
            let oldest = self.records.iter().min_by_key(|(_, r)| r.order).map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(status.id.clone(), Record { status, order: self.clock });
        true
    }

    fn clear(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return self.reset();
        }
        let before = self.records.len();
        self.records.retain(|key, _| {
            key != id && !key.strip_prefix(id).is_some_and(|tail| tail.starts_with('/'))
        });
        self.records.len() != before
    }

    fn retain(&mut self, keep: impl Fn(&ProgramStatus) -> bool) -> bool {
        let before = self.records.len();
        self.records.retain(|_, record| keep(&record.status));
        self.records.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(body: &str) -> ProgramStatus {
        match parse_report(body) {
            Some(ProgramReport::Status(status)) => status,
            other => panic!("{body}: {other:?}"),
        }
    }

    fn feed(tracker: &mut ProgramTracker, body: &str) -> bool {
        tracker.apply(ProgramEvent::Report(parse_report(body).unwrap()))
    }

    #[test]
    fn reports_from_claude_code_and_codex() {
        let working = status("state=working:app=claude-code");
        assert_eq!(working.state, ProgramState::Working);
        assert_eq!(working.app.as_deref(), Some("claude-code"));
        assert_eq!(working.id, "");

        // Captured from Claude Code 2.1.319 asking through AskUserQuestion.
        let blocked = status(
            "state=blocked:app=claude-code:kind=question:\
             msg=YW5zd2VyOiDkvaDllpzmrKLnuqLoibLov5jmmK/ok53oibLvvJ8gKOe6ouiJsiDCtyDok53oibIp",
        );
        assert_eq!(blocked.kind, Some(InputKind::Question));
        assert_eq!(blocked.message.as_deref(), Some("answer: 你喜欢红色还是蓝色？ (红色 · 蓝色)"));

        assert_eq!(status("state=done:app=codex").state, ProgramState::Done);
        assert_eq!(status("state=working:progress=25").progress, Some(25));
        assert_eq!(status("state=working:progress=250").progress, None);
        // A kind only means something while blocked.
        assert_eq!(status("state=working:kind=question").kind, None);
    }

    #[test]
    fn malformed_reports_are_discarded_whole() {
        for body in [
            "app=codex",
            "state=sleeping",
            // Allowed characters, but not base64.
            "state=done:msg=a",
            // Valid base64 of text with a control character.
            "state=done:msg=YQpi",
            "state=done:id=a//b",
            "state=done:id=a/b/c/d/e/f/g/h/i",
            "state=done:app=a-very-long-application-name-indeed",
            "state=done:averyveryverylongkey=1",
        ] {
            assert_eq!(parse_report(body), None, "{body}");
        }
        let long_title = format!("state=done:title={}", "QUFB".repeat(70));
        assert_eq!(parse_report(&long_title), None);
        // Unknown and oddly spelled pairs are skipped, not fatal.
        assert_eq!(status("state=done:msg=!!!").message, None);
        assert_eq!(status("state=done:Weird=1:future=x").state, ProgramState::Done);
    }

    #[test]
    fn busy_means_working_or_waiting_mid_turn() {
        let mut tracker = ProgramTracker::default();
        feed(&mut tracker, "state=idle:app=claude-code");
        assert!(tracker.has_running() && !tracker.is_busy());
        feed(&mut tracker, "state=working:app=claude-code");
        assert!(tracker.is_busy());
        feed(&mut tracker, "state=blocked:app=claude-code:kind=permission");
        assert!(tracker.is_busy());
        // A finished subagent beside a working parent keeps the turn going.
        feed(&mut tracker, "state=done:id=sub");
        assert!(tracker.is_busy());
        feed(&mut tracker, "state=done:app=claude-code");
        assert!(!tracker.is_busy());
    }

    #[test]
    fn bidi_overrides_are_disarmed() {
        // "a\u{202e}b"
        assert_eq!(status("state=done:msg=YeKArmI=").message.as_deref(), Some("ab"));
    }

    #[test]
    fn summary_prefers_what_needs_the_user() {
        let mut tracker = ProgramTracker::default();
        assert!(feed(&mut tracker, "state=working:app=claude-code"));
        assert!(feed(&mut tracker, "state=done:id=agent/one"));
        assert_eq!(tracker.summary().unwrap().state, ProgramState::Working);

        assert!(feed(&mut tracker, "state=blocked:id=agent/two:kind=permission"));
        let summary = tracker.summary().unwrap();
        assert_eq!(summary.state, ProgramState::Blocked);
        // `agent` has no record, so the app comes from the root.
        assert_eq!(summary.app.as_deref(), Some("claude-code"));

        // Repeating a report changes nothing.
        assert!(!feed(&mut tracker, "state=blocked:id=agent/two:kind=permission"));
    }

    #[test]
    fn clearing_removes_a_subtree() {
        let mut tracker = ProgramTracker::default();
        feed(&mut tracker, "state=working:id=agent");
        feed(&mut tracker, "state=working:id=agent/child");
        feed(&mut tracker, "state=working:id=agentx");
        assert!(feed(&mut tracker, "state=clear:id=agent"));
        assert_eq!(tracker.summary().unwrap().id, "agentx");
        assert!(feed(&mut tracker, "state=clear"));
        assert!(tracker.is_empty());
    }

    #[test]
    fn lifecycle_keeps_unseen_results() {
        let mut tracker = ProgramTracker::default();
        feed(&mut tracker, "state=working:id=a");
        feed(&mut tracker, "state=done:id=b");
        // A new prompt ends what was running but keeps the result.
        assert!(tracker.apply(ProgramEvent::CommandStart));
        assert_eq!(tracker.summary().unwrap().state, ProgramState::Done);
        // Seeing it drops it.
        assert!(tracker.acknowledge());
        assert!(tracker.is_empty());

        feed(&mut tracker, "state=error");
        assert!(tracker.apply(ProgramEvent::Reset));
        assert!(tracker.is_empty());
    }

    #[test]
    fn record_count_is_bounded() {
        let mut tracker = ProgramTracker::default();
        for i in 0..=MAX_RECORDS {
            feed(&mut tracker, &format!("state=working:id=r{i}"));
        }
        assert_eq!(tracker.records.len(), MAX_RECORDS);
        // The least recently reported record made room.
        assert!(!tracker.records.contains_key("r0"));
    }

    #[test]
    fn probe_replies_are_bounded_per_window() {
        let mut limiter = ProbeLimiter::default();
        let start = Instant::now();
        assert!((0..MAX_PROBE_REPLIES).all(|_| limiter.allow(start)));
        assert!(!limiter.allow(start + Duration::from_secs(9)));
        assert!(limiter.allow(start + PROBE_WINDOW));
    }
}
