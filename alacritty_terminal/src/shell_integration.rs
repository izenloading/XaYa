//! Shell integration marks (`OSC 133`, from FinalTerm).
//!
//! A shell with integration installed brackets each prompt and command:
//!
//! ```text
//! OSC 133 ; A ST   prompt starts
//! OSC 133 ; B ST   prompt ends, the user types the command
//! OSC 133 ; C ST   the command runs, its output follows
//! OSC 133 ; D [; <exit code>] ST   the command finished
//! ```
//!
//! Further `;`-separated parameters (such as `aid=` or `cl=`) are accepted and ignored.

/// One `OSC 133` mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMark {
    /// `A`: the shell starts drawing its prompt.
    PromptStart,
    /// `B`: the prompt is drawn and the user types a command.
    InputStart,
    /// `C`: the command starts running.
    CommandStart,
    /// `D`: the command finished, with its exit code when the shell reports one.
    CommandEnd(Option<i32>),
}

/// Parse the part of an `OSC 133` payload after `133;`.
pub fn parse_mark(body: &str) -> Option<ShellMark> {
    let mut params = body.split(';');
    match params.next()? {
        "A" => Some(ShellMark::PromptStart),
        "B" => Some(ShellMark::InputStart),
        "C" => Some(ShellMark::CommandStart),
        "D" => Some(ShellMark::CommandEnd(params.next().and_then(|code| code.parse().ok()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_exit_codes() {
        assert_eq!(parse_mark("A"), Some(ShellMark::PromptStart));
        assert_eq!(parse_mark("A;cl=m;aid=123"), Some(ShellMark::PromptStart));
        assert_eq!(parse_mark("B"), Some(ShellMark::InputStart));
        assert_eq!(parse_mark("C"), Some(ShellMark::CommandStart));
        assert_eq!(parse_mark("D"), Some(ShellMark::CommandEnd(None)));
        assert_eq!(parse_mark("D;0"), Some(ShellMark::CommandEnd(Some(0))));
        assert_eq!(parse_mark("D;130;aid=1"), Some(ShellMark::CommandEnd(Some(130))));
        assert_eq!(parse_mark("D;x"), Some(ShellMark::CommandEnd(None)));
        assert_eq!(parse_mark("E"), None);
        assert_eq!(parse_mark(""), None);
        // Prefixes of other marks aren't marks.
        assert_eq!(parse_mark("AB"), None);
    }
}
