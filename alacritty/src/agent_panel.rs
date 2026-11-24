//! The agent panel: what the agents in every window are doing, in one list over the terminal, to
//! see who needs you and go there.

use std::time::Duration;

use unicode_width::UnicodeWidthChar;

use crate::program_status::{display_name, format_duration};
use crate::window_status::{TurnChanges, WindowStatus, sort_statuses};

/// Hint shown in the panel's header.
const HEADER: &str = "Agents   ↑↓ select · ⏎ go · d diff · p project · m managed · esc close";

/// Columns of the state, app, duration and turn changes fields.
const STATE_WIDTH: usize = 20;
const APP_WIDTH: usize = 12;
const DURATION_WIDTH: usize = 8;
const CHANGES_WIDTH: usize = 20;

/// Every window's agents, ordered by what needs the user first.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPanel {
    windows: Vec<WindowStatus>,
    /// The selected window's id.
    selected: Option<u64>,
    /// The window showing the panel.
    current: u64,
    pub managed: crate::managed_panel::ManagedPanel,
    pub managed_mode: bool,
}

/// How a line's text is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// Waiting on the user.
    Blocked,
    Error,
    Done,
    Working,
    /// Secondary text, such as a program's message.
    Dim,
}

/// Which kind of line, deciding its background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Header,
    Window { selected: bool },
}

/// Text starting at a column.
///
/// Wide characters are followed by a spacer the renderer skips, so `text` has one `char` per
/// column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub column: usize,
    pub text: String,
    pub tone: Tone,
}

/// One line of the panel, filling the window's width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelLine {
    pub kind: LineKind,
    pub segments: Vec<Segment>,
}

impl AgentPanel {
    /// A panel shown in window `current`, starting on the most urgent other window, since that's
    /// where the user is likely headed.
    pub fn new(current: u64, windows: Vec<WindowStatus>) -> Self {
        let mut panel = Self {
            windows: Vec::new(),
            selected: None,
            current,
            managed: Default::default(),
            managed_mode: false,
        };
        panel.update(windows);
        let other = panel.windows.iter().find(|window| window.window_id != current);
        panel.selected = other.or(panel.windows.first()).map(|window| window.window_id);
        panel
    }

    /// Replace the windows, keeping the selection on the same window while it exists.
    ///
    /// Returns whether anything shown changed.
    pub fn update(&mut self, mut windows: Vec<WindowStatus>) -> bool {
        sort_statuses(&mut windows);
        let changed = windows != self.windows;
        self.windows = windows;

        let selected_exists = self
            .selected
            .is_some_and(|selected| self.windows.iter().any(|window| window.window_id == selected));
        if !selected_exists {
            self.selected = self.windows.first().map(|window| window.window_id);
        }
        changed
    }

    /// The selected window.
    pub fn selected(&self) -> Option<u64> {
        if self.managed_mode { None } else { self.selected }
    }

    /// Move the selection `delta` rows, wrapping around.
    pub fn select_relative(&mut self, delta: isize) {
        if self.managed_mode {
            self.managed.select_relative(delta);
            return;
        }
        let len = self.windows.len() as isize;
        if len == 0 {
            return;
        }
        let index = self.selected_index().unwrap_or(0) as isize;
        let index = (index + delta).rem_euclid(len) as usize;
        self.selected = Some(self.windows[index].window_id);
    }

    /// Select the window in row `index`, counting from zero, returning whether it exists.
    pub fn select_index(&mut self, index: usize) -> bool {
        if self.managed_mode {
            return self.managed.select_index(index);
        }
        match self.windows.get(index) {
            Some(window) => {
                self.selected = Some(window.window_id);
                true
            },
            None => false,
        }
    }

    /// What the selected window's agent changed in its last turn.
    pub fn selected_changes(&self) -> Option<&TurnChanges> {
        if self.managed_mode {
            return None;
        }
        self.windows.get(self.selected_index()?)?.changes.as_deref()
    }

    fn selected_index(&self) -> Option<usize> {
        self.windows.iter().position(|window| Some(window.window_id) == self.selected)
    }

    /// The panel's lines, `columns` wide and at most `max_lines` of them.
    ///
    /// Each window takes a line, and one more for its program's message. When they don't all fit,
    /// the rows shown scroll to keep the selection visible.
    pub fn lines(&self, columns: usize, max_lines: usize) -> Vec<PanelLine> {
        if self.managed_mode {
            return self.managed.lines(columns, max_lines);
        }
        if max_lines == 0 || columns == 0 {
            return Vec::new();
        }

        let header = PanelLine {
            kind: LineKind::Header,
            segments: vec![Segment {
                column: 0,
                text: fit(&format!(" {HEADER}"), columns),
                tone: Tone::Plain,
            }],
        };

        let rows: Vec<_> = self
            .windows
            .iter()
            .enumerate()
            .map(|(i, window)| self.window_lines(i, window, columns))
            .collect();

        // Scroll so the selection fits, preferring to start at the top.
        let budget = max_lines - 1;
        let selected = self.selected_index().unwrap_or(0);
        let mut first = 0;
        while first < selected
            && rows[first..=selected].iter().map(Vec::len).sum::<usize>() > budget
        {
            first += 1;
        }

        let mut lines = vec![header];
        for row in &rows[first..] {
            if lines.len() + row.len() > max_lines {
                break;
            }
            lines.extend(row.iter().cloned());
        }
        lines
    }

    /// The line describing `window` in row `index`, and its message's line.
    fn window_lines(&self, index: usize, window: &WindowStatus, columns: usize) -> Vec<PanelLine> {
        let selected = Some(window.window_id) == self.selected;
        let kind = LineKind::Window { selected };

        let tone = match window.state.as_deref() {
            Some("blocked") => Tone::Blocked,
            Some("error") => Tone::Error,
            Some("done") => Tone::Done,
            Some("working") => Tone::Working,
            _ => Tone::Dim,
        };
        let number = if index < 9 { format!("{}", index + 1) } else { String::from(" ") };
        let app = window.app.as_deref().map_or("-", display_name);
        let duration = window
            .seconds
            .map_or_else(|| "-".into(), |seconds| format_duration(Duration::from_secs(seconds)));
        let changes = window.changes.as_deref().map(TurnChanges::label).unwrap_or_default();
        let mut title = window.title.clone();
        if window.window_id == self.current {
            title.push_str("  (this window)");
        }

        // Fields left to right; each field gets what the earlier ones left of the width.
        let changes_column = 4 + STATE_WIDTH + APP_WIDTH + DURATION_WIDTH;
        let fields = [
            (1, number, Tone::Dim, 3),
            (4, window.state_label(), tone, STATE_WIDTH),
            (4 + STATE_WIDTH, app.to_owned(), Tone::Plain, APP_WIDTH),
            (4 + STATE_WIDTH + APP_WIDTH, duration, Tone::Plain, DURATION_WIDTH),
            (changes_column, changes, Tone::Dim, CHANGES_WIDTH),
            (changes_column + CHANGES_WIDTH, title, Tone::Plain, usize::MAX),
        ];
        let mut segments = Vec::new();
        for (column, text, tone, width) in fields {
            if column >= columns {
                break;
            }
            let width = width.saturating_sub(1).min(columns - column).max(1);
            segments.push(Segment { column, text: fit(&text, width), tone });
        }

        let mut lines = vec![PanelLine { kind, segments }];
        if let Some(message) = &window.message {
            let column = 4.min(columns - 1);
            let text = fit(&message.replace('\n', " "), columns - column);
            lines.push(PanelLine {
                kind,
                segments: vec![Segment { column, text, tone: Tone::Dim }],
            });
        }
        lines
    }
}

/// `text` as at most `width` columns: a spacer after each wide character, control and zero-width
/// characters dropped, and `…` ending it when it's cut short.
pub(crate) fn fit(text: &str, width: usize) -> String {
    let mut fitted = String::new();
    let mut used = 0;
    let chars: Vec<_> = text.chars().filter(|c| !c.is_control()).collect();
    for (i, &c) in chars.iter().enumerate() {
        let c_width = match c.width() {
            Some(c_width) if c_width > 0 => c_width,
            _ => continue,
        };
        // Keep a column for the ellipsis unless this is the last character.
        let rest_fits = i + 1 == chars.len() || used + c_width < width;
        if used + c_width > width || !rest_fits {
            if used < width {
                fitted.push('…');
            }
            return fitted;
        }
        fitted.push(c);
        if c_width == 2 {
            fitted.push(' ');
        }
        used += c_width;
    }
    fitted
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alacritty_terminal::program_status::{ProgramEvent, parse_report};

    use super::*;
    use crate::program_status::ProgramActivity;

    fn window(id: u64, title: &str, reports: &[&str]) -> WindowStatus {
        let mut activity = ProgramActivity::default();
        for body in reports {
            activity.handle(ProgramEvent::Report(parse_report(body).unwrap()), false);
        }
        WindowStatus::new(id, title.into(), false, &activity, Instant::now())
    }

    fn windows() -> Vec<WindowStatus> {
        vec![
            window(1, "shell", &[]),
            window(2, "api", &["state=working:app=codex"]),
            // "approve?" in base64.
            window(3, "web", &[
                "state=working:app=claude-code",
                "state=blocked:app=claude-code:kind=permission:msg=YXBwcm92ZT8=",
            ]),
        ]
    }

    fn text(line: &PanelLine) -> String {
        let mut text = String::new();
        for segment in &line.segments {
            while text.chars().count() < segment.column {
                text.push(' ');
            }
            text.push_str(&segment.text);
        }
        text
    }

    #[test]
    fn urgent_windows_come_first_and_start_selected() {
        let panel = AgentPanel::new(1, windows());
        let order: Vec<_> = panel.windows.iter().map(|window| window.window_id).collect();
        assert_eq!(order, [3, 2, 1]);
        assert_eq!(panel.selected(), Some(3));

        // From the most urgent window itself, the next one is where to go.
        assert_eq!(AgentPanel::new(3, windows()).selected(), Some(2));
    }

    #[test]
    fn selection_wraps_and_follows_its_window() {
        let mut panel = AgentPanel::new(1, windows());
        panel.select_relative(-1);
        assert_eq!(panel.selected(), Some(1));
        panel.select_relative(1);
        assert_eq!(panel.selected(), Some(3));
        assert!(panel.select_index(1));
        assert!(!panel.select_index(3));
        assert_eq!(panel.selected(), Some(2));

        // Window 2 finishes and moves up; the selection stays on it.
        let mut updated = windows();
        updated[1] = window(2, "api", &["state=working:app=codex", "state=error:app=codex"]);
        assert!(panel.update(updated.clone()));
        assert_eq!(panel.selected(), Some(2));
        assert!(!panel.update(updated));

        // Window 2 closes; the selection falls back to the top.
        assert!(panel.update(vec![window(1, "shell", &[])]));
        assert_eq!(panel.selected(), Some(1));
    }

    #[test]
    fn lines_describe_each_window() {
        let panel = AgentPanel::new(1, windows());
        let lines = panel.lines(100, 20);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].kind, LineKind::Header);
        assert!(text(&lines[0]).starts_with(" Agents"));

        let blocked = text(&lines[1]);
        assert!(blocked.starts_with(" 1  waiting: permission"), "{blocked}");
        assert!(blocked.contains("Claude Code"));
        assert!(blocked.ends_with("web"));
        assert_eq!(lines[1].kind, LineKind::Window { selected: true });
        assert_eq!(lines[1].segments[1].tone, Tone::Blocked);
        assert_eq!(text(&lines[2]).trim(), "approve?");
        assert_eq!(lines[2].kind, LineKind::Window { selected: true });

        assert!(text(&lines[3]).contains("working"));
        assert_eq!(lines[3].kind, LineKind::Window { selected: false });
        assert!(text(&lines[4]).ends_with("shell  (this window)"));

        // Every line fits the width.
        for line in panel.lines(30, 20) {
            assert!(text(&line).chars().count() <= 30, "{}", text(&line));
        }
    }

    #[test]
    fn lines_show_what_the_last_turn_changed() {
        let mut windows = windows();
        windows[1].changes = Some(Box::new(TurnChanges {
            files: 3,
            insertions: 48,
            deletions: 3,
            root: "/repo".into(),
            before: "a".into(),
            after: "b".into(),
        }));
        let mut panel = AgentPanel::new(1, windows);
        let lines = panel.lines(100, 20);
        let working = text(&lines[3]);
        assert!(working.contains("3 files +48 -3"), "{working}");
        assert!(working.ends_with("api"), "{working}");

        // Only the window with changes has a diff to open.
        assert_eq!(panel.selected(), Some(3));
        assert_eq!(panel.selected_changes(), None);
        panel.select_index(1);
        assert_eq!(panel.selected_changes().map(|changes| changes.files), Some(3));
    }

    #[test]
    fn lines_scroll_to_keep_the_selection_visible() {
        let windows: Vec<_> = (1..=10).map(|id| window(id, &format!("w{id}"), &[])).collect();
        let mut panel = AgentPanel::new(1, windows);
        assert_eq!(panel.lines(80, 4).len(), 4);

        panel.select_index(9);
        let lines = panel.lines(80, 4);
        assert_eq!(lines.len(), 4);
        assert!(text(&lines[3]).ends_with("w10"));
        assert_eq!(lines[3].kind, LineKind::Window { selected: true });
        assert!(panel.lines(80, 0).is_empty());
    }

    #[test]
    fn text_fits_its_columns() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello", 5), "hello");
        assert_eq!(fit("hello world", 5), "hell…");
        // Wide characters take two columns and get a spacer.
        assert_eq!(fit("你好", 4), "你 好 ");
        assert_eq!(fit("你好吗", 4), "你 …");
        assert_eq!(fit("a\u{7}b\u{200b}", 5), "ab");
        assert_eq!(fit("abc", 1), "…");
    }
}
