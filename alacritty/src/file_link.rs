//! File locations such as `src/main.rs:12:5` in terminal output, as coding agents and compilers
//! print them, for the built-in `OpenFile` hint action.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::ui_config::Program;

/// Default hint regex for file locations.
///
/// Either a path with at least one `/` and an optional `:line[:column]`, or a bare file name with
/// an extension and a line number. Plain words like `and/or` match too; they're filtered out when
/// triggered, by checking that the file exists.
pub const FILE_REGEX: &str = "(~|\\.\\.?)?/?([A-Za-z0-9_@.+-]+/)+[A-Za-z0-9_@.+-]*[A-Za-z0-9_@\
                              +-](:[0-9]+){0,2}|[A-Za-z0-9_@+-][A-Za-z0-9_@.+-]*\\.[A-Za-z0-9]+:\
                              [0-9]+(:[0-9]+)?";

/// A file and an optional position in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLink {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl FileLink {
    /// Parse `path[:line[:column]]`, resolving the path against `cwd` and `~` against `home`.
    pub fn parse(text: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<Self> {
        // Split off up to two trailing `:<number>`s; earlier colons belong to the path.
        let mut path = text.trim();
        let mut numbers = Vec::with_capacity(2);
        while numbers.len() < 2 {
            match path.rsplit_once(':') {
                Some((head, tail))
                    if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    numbers.push(tail.parse::<u32>().ok()?);
                    path = head;
                },
                _ => break,
            }
        }
        if path.is_empty() {
            return None;
        }
        let (line, column) = match numbers[..] {
            [column, line] => (Some(line), Some(column)),
            [line] => (Some(line), None),
            _ => (None, None),
        };

        let path = match path.strip_prefix("~/") {
            Some(tail) => home?.join(tail),
            None => match cwd {
                Some(cwd) if Path::new(path).is_relative() => cwd.join(path),
                _ => PathBuf::from(path),
            },
        };

        // Line zero means no line, and a column needs its line.
        let line = line.filter(|&line| line > 0);
        Some(Self { path, line, column: column.filter(|_| line.is_some()) })
    }

    /// `path:line:column` as most editors (VS Code `-g`, Cursor, Zed, Sublime) take it.
    pub fn editor_argument(&self) -> String {
        let mut argument = self.path.display().to_string();
        if let Some(line) = self.line {
            argument.push_str(&format!(":{line}"));
            if let Some(column) = self.column {
                argument.push_str(&format!(":{column}"));
            }
        }
        argument
    }

    /// The program and arguments opening this file: `editor` with `path:line:column` appended,
    /// or the system's default application for it without a position.
    pub fn open_command(self, editor: Option<&Program>) -> (String, Vec<OsString>) {
        if let Some(editor) = editor {
            let mut args: Vec<OsString> = editor.args().iter().map(OsString::from).collect();
            args.push(self.editor_argument().into());
            return (editor.program().to_owned(), args);
        }

        let path = self.path.into_os_string();
        #[cfg(target_os = "macos")]
        return ("open".into(), vec![path]);
        #[cfg(not(any(target_os = "macos", windows)))]
        return ("xdg-open".into(), vec![path]);
        #[cfg(windows)]
        return ("cmd".into(), vec!["/c".into(), "start".into(), "".into(), path]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> FileLink {
        FileLink::parse(text, Some(Path::new("/repo")), Some(Path::new("/home/me"))).unwrap()
    }

    #[test]
    fn locations_resolve_against_the_working_directory() {
        let link = parse("src/main.rs:12:5");
        assert_eq!(link.path, Path::new("/repo/src/main.rs"));
        assert_eq!((link.line, link.column), (Some(12), Some(5)));
        assert_eq!(link.editor_argument(), "/repo/src/main.rs:12:5");

        assert_eq!(parse("./a/b.rs").path, Path::new("/repo/./a/b.rs"));
        assert_eq!(parse("/etc/hosts:3").path, Path::new("/etc/hosts"));
        assert_eq!(parse("~/x/y.txt").path, Path::new("/home/me/x/y.txt"));

        let bare = parse("Cargo.toml");
        assert_eq!((bare.line, bare.column), (None, None));
        assert_eq!(bare.editor_argument(), "/repo/Cargo.toml");

        // Line zero means no line, and takes its column with it.
        let zero = parse("a/b:0:4");
        assert_eq!((zero.line, zero.column), (None, None));
    }

    #[test]
    fn colons_inside_paths_survive() {
        let link = parse("a:b/c.rs:7");
        assert_eq!(link.path, Path::new("/repo/a:b/c.rs"));
        assert_eq!(link.line, Some(7));

        assert!(FileLink::parse("", None, None).is_none());
        assert!(FileLink::parse(":12", None, None).is_none());
        // `~` needs a home to resolve against.
        assert!(FileLink::parse("~/a", None, None).is_none());
    }

    #[test]
    fn editors_get_the_position() {
        let editor = Program::WithArgs { program: "code".into(), args: vec!["-g".into()] };
        let (program, args) = parse("src/lib.rs:3:9").open_command(Some(&editor));
        assert_eq!(program, "code");
        assert_eq!(args, ["-g", "/repo/src/lib.rs:3:9"]);

        let (_, args) = parse("src/lib.rs:3:9").open_command(None);
        assert_eq!(args.last().unwrap(), "/repo/src/lib.rs");
    }
}
