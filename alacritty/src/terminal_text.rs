//! A window's text as `msg get-text` reads it.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::Term;

/// Which part of the terminal to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRange {
    /// The screen, as the program drew it, regardless of where the view is scrolled.
    Screen,
    /// The last rows with content, scrollback included.
    Lines(usize),
    /// The last shell command run before the current prompt and its output, from shell
    /// integration (`OSC 133`) marks.
    LastCommand,
}

/// The text in `range`, trailing blank lines removed; `None` when the range doesn't exist, such as
/// a last command without shell integration.
pub fn text<T>(term: &Term<T>, range: TextRange) -> Option<String> {
    let bottom = term.bottommost_line();
    let (start, end) = match range {
        TextRange::Screen => (Line(0), bottom),
        TextRange::Lines(count) => {
            let end = last_content_line(term)?;
            let count = i32::try_from(count).unwrap_or(i32::MAX).max(1);
            let start = Line(end.0.saturating_sub(count - 1)).max(term.topmost_line());
            (start, end)
        },
        TextRange::LastCommand => {
            // The prompt at or above the cursor is the current one; the command is above it.
            let cursor = term.grid().cursor.point.line;
            let current = term.prompt_line(cursor + 1, Direction::Left)?;
            let previous = term.prompt_line(current, Direction::Left)?;
            (previous, current - 1)
        },
    };

    let end = Point::new(end, term.last_column());
    let text = term.bounds_to_string(Point::new(start, Column(0)), end);
    Some(text.trim_end().to_owned())
}

/// The bottommost line with any content, scrollback included.
fn last_content_line<T>(term: &Term<T>) -> Option<Line> {
    let grid = term.grid();
    (term.topmost_line().0..=term.bottommost_line().0)
        .rev()
        .map(Line)
        .find(|&line| !grid[line].is_clear())
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::{TermSize, mock_term};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Handler;

    use super::*;

    fn write(term: &mut Term<VoidListener>, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => {
                    term.carriage_return();
                    term.newline();
                },
                c => term.input(c),
            }
        }
    }

    /// A prompt bracketed by shell integration marks, then `command` and its `output`.
    fn command(term: &mut Term<VoidListener>, command: &str, output: &str) {
        term.mark_prompt_start();
        write(term, "$ ");
        term.mark_prompt_end();
        write(term, &format!("{command}\n{output}"));
    }

    #[test]
    fn screen_and_last_lines() {
        // Lines without `\r` are soft-wrapped into the next one.
        let term = mock_term("one\r\ntwo\r\nthree\r\n\r\n");
        assert_eq!(text(&term, TextRange::Screen).unwrap(), "one\ntwo\nthree");
        // Blank lines at the bottom don't count.
        assert_eq!(text(&term, TextRange::Lines(2)).unwrap(), "two\nthree");
        assert_eq!(text(&term, TextRange::Lines(100)).unwrap(), "one\ntwo\nthree");
        assert_eq!(text(&mock_term("\r\n\r\n"), TextRange::Lines(5)), None);
    }

    #[test]
    fn last_lines_reach_into_scrollback() {
        let mut term = Term::new(Config::default(), &TermSize::new(10, 3), VoidListener);
        write(&mut term, "a\nb\nc\nd\ne");
        assert_eq!(text(&term, TextRange::Screen).unwrap(), "c\nd\ne");
        assert_eq!(text(&term, TextRange::Lines(4)).unwrap(), "b\nc\nd\ne");
    }

    #[test]
    fn last_command_needs_shell_integration() {
        let mut term = Term::new(Config::default(), &TermSize::new(20, 10), VoidListener);
        assert_eq!(text(&term, TextRange::LastCommand), None);

        command(&mut term, "ls", "a.rs\nb.rs\n");
        command(&mut term, "cargo test", "ok\n");
        // Only one prompt so far is complete: the second is the current one... until a third.
        term.mark_prompt_start();
        write(&mut term, "$ ");
        term.mark_prompt_end();

        assert_eq!(text(&term, TextRange::LastCommand).unwrap(), "$ cargo test\nok");
    }
}
