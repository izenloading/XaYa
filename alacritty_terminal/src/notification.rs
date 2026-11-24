//! Requests to the terminal embedded in the PTY output that `vte` doesn't dispatch.
//!
//! `vte` drops OSC sequences it doesn't know, so these are picked out by a
//! small streaming scanner that runs beside the parser over the same bytes. It
//! observes the stream without rewriting it, so the emulator sees exactly what
//! it would without the scanner.
//!
//! Supported forms:
//!
//! - `OSC 9 ; <body> ST` (iTerm2 / Windows Terminal). ConEmu reuses `OSC 9` for numbered
//!   sub-commands (`9;4;…` progress, `9;9;…` cwd, …), so a body whose first field is a ConEmu
//!   command number is ignored.
//! - `OSC 777 ; notify ; <title> ; <body> ST` (urxvt / VTE / foot / Ghostty).
//! - `OSC 7501 ; ? ST` and `OSC 7501 ; <report> ST`: the Program Status Protocol, see
//!   [`crate::program_status`]. `RIS` is picked out too, since it ends what programs reported.
//! - `OSC 133 ; <mark> ST`: shell integration marks, see [`crate::shell_integration`].
//! - `OSC 21337 ; <status> ST`: tab status reports, passed on raw for now.
//!
//! Sequences may be terminated by `BEL` or `ESC \` and may be split across
//! arbitrary read chunks. Each sequence is bounded to [`MAX_SEQUENCE`] bytes;
//! longer ones are discarded whole. OSC-shaped data inside other control
//! strings (DCS, SOS, PM, APC) is ignored.

/// Largest OSC payload considered, in bytes.
pub const MAX_SEQUENCE: usize = 4096;

/// Longest title kept, in characters.
const MAX_TITLE_CHARS: usize = 256;

/// Longest body kept, in characters.
const MAX_BODY_CHARS: usize = 1024;

/// A complete request picked out of the PTY output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sequence {
    /// `OSC 9` / `OSC 777` desktop notification.
    Notification(Notification),

    /// `OSC 7501 ; ?`: a program asking whether the terminal understands status reports.
    ProgramStatusQuery,

    /// `OSC 7501 ; <report>` or `RIS`.
    ProgramStatus(ProgramEvent),

    /// `OSC 133` shell integration mark.
    Shell(ShellMark),

    /// `OSC 21337 ; <status>`, without the `21337;` prefix.
    TabStatus(String),
}

use crate::program_status::{self, ProgramEvent};
use crate::shell_integration::{self, ShellMark};

/// A desktop notification requested by the program running in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Notification title; `None` when the program only supplied a body.
    pub title: Option<String>,

    /// Notification body.
    pub body: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    /// DCS, SOS, PM or APC: skipped until its terminator.
    String,
    StringEscape,
}

/// Streaming scanner for the OSC sequences in [`Sequence`].
#[derive(Debug, Default)]
pub struct NotificationScanner {
    scan: Scan,
    osc: Vec<u8>,
    oversized: bool,
}

impl NotificationScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed PTY output, calling `on_sequence` for every complete request.
    ///
    /// The callback also gets the offset in `bytes` just past the request's terminator, so a
    /// query can be answered in stream order relative to the queries `vte` answers.
    pub fn advance<F: FnMut(usize, Sequence)>(&mut self, bytes: &[u8], mut on_sequence: F) {
        let mut i = 0;
        while i < bytes.len() {
            // Outside a sequence only `ESC` matters, so skip straight to the next one. Plain
            // output is most of what a terminal reads.
            if self.scan == Scan::Ground {
                match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(offset) => {
                        i += offset + 1;
                        self.scan = Scan::Escape;
                        continue;
                    },
                    None => return,
                }
            }

            let byte = bytes[i];
            i += 1;
            let end = i;
            match self.scan {
                Scan::Ground => unreachable!("ground bytes are skipped above"),
                Scan::Escape => self.escape(end, byte, &mut on_sequence),
                Scan::Osc => match byte {
                    0x07 => self.finish(end, &mut on_sequence),
                    0x1b => self.scan = Scan::OscEscape,
                    // CAN and SUB abort the sequence.
                    0x18 | 0x1a => self.scan = Scan::Ground,
                    _ if self.osc.len() < MAX_SEQUENCE => self.osc.push(byte),
                    _ => self.oversized = true,
                },
                Scan::OscEscape => {
                    if byte == b'\\' {
                        self.finish(end, &mut on_sequence);
                    } else {
                        // Any other escape cancels the unfinished sequence.
                        self.escape(end, byte, &mut on_sequence);
                    }
                },
                Scan::String => match byte {
                    0x07 | 0x18 | 0x1a => self.scan = Scan::Ground,
                    0x1b => self.scan = Scan::StringEscape,
                    _ => (),
                },
                Scan::StringEscape => {
                    self.scan = if byte == b'\\' { Scan::Ground } else { Scan::String };
                },
            }
        }
    }

    fn escape<F: FnMut(usize, Sequence)>(&mut self, end: usize, byte: u8, on_sequence: &mut F) {
        if byte == b'c' {
            on_sequence(end, Sequence::ProgramStatus(ProgramEvent::Reset));
        }
        self.scan = match byte {
            b']' => {
                self.osc.clear();
                self.oversized = false;
                Scan::Osc
            },
            b'P' | b'X' | b'^' | b'_' => Scan::String,
            0x1b => Scan::Escape,
            _ => Scan::Ground,
        };
    }

    fn finish<F: FnMut(usize, Sequence)>(&mut self, end: usize, on_sequence: &mut F) {
        self.scan = Scan::Ground;
        if !self.oversized
            && let Some(sequence) = parse(&self.osc)
        {
            on_sequence(end, sequence);
        }
        self.osc.clear();
        self.oversized = false;
    }
}

/// Parse a complete OSC payload (without the introducer and terminator).
fn parse(payload: &[u8]) -> Option<Sequence> {
    let payload = std::str::from_utf8(payload).ok()?;
    let (command, rest) = payload.split_once(';')?;
    match command {
        "7501" if rest == "?" => Some(Sequence::ProgramStatusQuery),
        "7501" => program_status::parse_report(rest)
            .map(|report| Sequence::ProgramStatus(ProgramEvent::Report(report))),
        "133" => shell_integration::parse_mark(rest).map(Sequence::Shell),
        "21337" => report(rest).map(Sequence::TabStatus),
        command => parse_notification(command, rest).map(Sequence::Notification),
    }
}

/// A raw tab status report, or `None` when it's empty or holds anything but printable ASCII.
fn report(body: &str) -> Option<String> {
    let valid = !body.is_empty() && body.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    valid.then(|| body.to_owned())
}

/// Parse an `OSC 9` / `OSC 777` notification.
fn parse_notification(command: &str, rest: &str) -> Option<Notification> {
    match command {
        "9" => {
            if is_conemu_command(rest) {
                return None;
            }
            let body = sanitize(rest, MAX_BODY_CHARS);
            (!body.is_empty()).then_some(Notification { title: None, body })
        },
        "777" => {
            let rest = rest.strip_prefix("notify;")?;
            let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
            let title = sanitize(title, MAX_TITLE_CHARS);
            let body = sanitize(body, MAX_BODY_CHARS);
            if title.is_empty() && body.is_empty() {
                return None;
            }
            Some(Notification { title: (!title.is_empty()).then_some(title), body })
        },
        _ => None,
    }
}

/// Whether an `OSC 9` body is a ConEmu sub-command (`1`..=`12`, optionally
/// followed by `;` and arguments) rather than notification text.
fn is_conemu_command(body: &str) -> bool {
    let first = body.split(';').next().unwrap_or_default();
    matches!(first.parse::<u8>(), Ok(1..=12)) && first.bytes().all(|b| b.is_ascii_digit())
}

/// Strip control and bidi-override characters and bound the length.
fn sanitize(text: &str, max_chars: usize) -> String {
    let cleaned: String =
        text.chars().filter(|&c| !c.is_control() && !is_bidi_control(c)).take(max_chars).collect();
    cleaned.trim().to_owned()
}

/// Whether `c` overrides or isolates text direction, which can disguise displayed text.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequences(chunks: &[&[u8]]) -> Vec<Sequence> {
        let mut scanner = NotificationScanner::new();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.advance(chunk, |_, sequence| out.push(sequence));
        }
        out
    }

    fn scan(chunks: &[&[u8]]) -> Vec<Notification> {
        sequences(chunks)
            .into_iter()
            .filter_map(|sequence| match sequence {
                Sequence::Notification(notification) => Some(notification),
                _ => None,
            })
            .collect()
    }

    fn body(text: &str) -> Notification {
        Notification { title: None, body: text.into() }
    }

    #[test]
    fn osc9_with_bel_and_st() {
        assert_eq!(scan(&[b"\x1b]9;build done\x07"]), vec![body("build done")]);
        assert_eq!(scan(&[b"\x1b]9;tests passed\x1b\\"]), vec![body("tests passed")]);
    }

    #[test]
    fn osc777_title_and_body() {
        let got = scan(&[b"\x1b]777;notify;Claude;Waiting; for input\x07"]);
        assert_eq!(got, vec![Notification {
            title: Some("Claude".into()),
            body: "Waiting; for input".into()
        }]);
        assert_eq!(scan(&[b"\x1b]777;notify;Only title\x07"]), vec![Notification {
            title: Some("Only title".into()),
            body: String::new()
        }]);
        assert!(scan(&[b"\x1b]777;other;x;y\x07"]).is_empty());
        assert!(scan(&[b"\x1b]777;notify;;\x07"]).is_empty());
    }

    #[test]
    fn split_across_chunks() {
        let got = scan(&[b"text\x1b", b"]9;he", b"llo\x1b", b"\\more"]);
        assert_eq!(got, vec![body("hello")]);
    }

    #[test]
    fn conemu_subcommands_are_ignored() {
        assert!(scan(&[b"\x1b]9;4;1;50\x07"]).is_empty());
        assert!(scan(&[b"\x1b]9;9;/tmp\x07"]).is_empty());
        assert!(scan(&[b"\x1b]9;12\x07"]).is_empty());
        // Numbers outside ConEmu's range are plain text.
        assert_eq!(scan(&[b"\x1b]9;42\x07"]), vec![body("42")]);
    }

    #[test]
    fn other_osc_and_strings_are_ignored() {
        assert!(scan(&[b"\x1b]0;title\x07\x1b]2;x\x1b\\"]).is_empty());
        // An OSC-looking payload inside a DCS string is not an OSC.
        assert!(scan(&[b"\x1bP\x1b]9;nope\x07rest\x1b\\"]).is_empty());
        // And scanning resumes after it.
        assert_eq!(scan(&[b"\x1bPq#0\x1b\\\x1b]9;yes\x07"]), vec![body("yes")]);
    }

    #[test]
    fn cancelled_and_oversized_sequences_are_dropped() {
        assert!(scan(&[b"\x1b]9;abc\x18\x07"]).is_empty());
        assert!(scan(&[b"\x1b]9;abc\x1b[0m"]).is_empty());
        let mut long = b"\x1b]9;".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_SEQUENCE + 10));
        long.push(0x07);
        assert!(scan(&[&long]).is_empty());
        // The scanner recovers for the next sequence.
        assert_eq!(scan(&[&long, b"\x1b]9;ok\x07"]), vec![body("ok")]);
    }

    #[test]
    fn control_and_bidi_characters_are_stripped() {
        let got = scan(&["\x1b]9;a\u{202e}b\tc\x07".as_bytes()]);
        assert_eq!(got, vec![body("abc")]);
        assert!(scan(&[b"\x1b]9;   \x07"]).is_empty());
        assert!(scan(&[b"\x1b]9;\xff\xfe\x07"]).is_empty());
    }

    #[test]
    fn program_status_query_and_reports() {
        assert_eq!(sequences(&[b"\x1b]7501;?\x1b\\"]), vec![Sequence::ProgramStatusQuery]);
        // A probe split across reads is still a probe.
        assert_eq!(sequences(&[b"\x1b]75", b"01;", b"?\x07"]), vec![Sequence::ProgramStatusQuery]);
        let report = program_status::parse_report("state=working:progress=25").unwrap();
        assert_eq!(sequences(&[b"\x1b]7501;state=working:progress=25\x07"]), vec![
            Sequence::ProgramStatus(ProgramEvent::Report(report))
        ]);
        assert_eq!(sequences(&[b"\x1b]21337;status=busy;indicator=spinner\x07"]), vec![
            Sequence::TabStatus("status=busy;indicator=spinner".into())
        ]);
    }

    #[test]
    fn malformed_status_reports_are_dropped() {
        assert!(sequences(&[b"\x1b]7501;\x07"]).is_empty());
        assert!(sequences(&[b"\x1b]7501;state=sleeping\x07"]).is_empty());
        assert!(sequences(&[b"\x1b]21337;a\tb\x07"]).is_empty());
        // Neither protocol is a notification.
        assert!(scan(&[b"\x1b]7501;state=done\x07"]).is_empty());
    }

    #[test]
    fn shell_marks_and_resets() {
        let marks = sequences(&[
            b"\x1b]133;A;cl=m\x1b\\$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\x1b]133;D;0\x07",
        ]);
        assert_eq!(marks, vec![
            Sequence::Shell(ShellMark::PromptStart),
            Sequence::Shell(ShellMark::InputStart),
            Sequence::Shell(ShellMark::CommandStart),
            Sequence::Shell(ShellMark::CommandEnd(Some(0))),
        ]);
        assert!(sequences(&[b"\x1b]133;Z\x07"]).is_empty());

        let reset = Sequence::ProgramStatus(ProgramEvent::Reset);
        assert_eq!(sequences(&[b"text\x1bc"]), vec![reset.clone()]);
        // RIS also cancels an unfinished OSC.
        assert_eq!(sequences(&[b"\x1b]9;abc\x1bc"]), vec![reset]);
        // Other escapes aren't resets.
        assert!(sequences(&[b"\x1b7\x1b8\x1b(B"]).is_empty());
    }

    #[test]
    fn reply_announces_every_state_and_kind() {
        let reply = program_status::SUPPORT_REPLY;
        assert!(reply.starts_with("\x1b]7501;?:") && reply.ends_with("\x1b\\"));
        for state in ["idle", "working", "done", "blocked", "error"] {
            assert!(reply.contains(state), "{state}");
        }
        for kind in ["permission", "question", "auth"] {
            assert!(reply.contains(kind), "{kind}");
        }
    }

    #[test]
    fn offsets_point_past_each_terminator() {
        let mut scanner = NotificationScanner::new();
        let bytes = b"ab\x1b]7501;?\x07\x1b[c\x1b]9;x\x1b\\";
        let mut ends = Vec::new();
        scanner.advance(bytes, |end, _| ends.push(end));
        // The probe ends before the DA1 query that follows it.
        assert_eq!(&bytes[..ends[0]], b"ab\x1b]7501;?\x07");
        assert_eq!(ends[1], bytes.len());
    }
}
