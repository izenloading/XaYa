//! Restoring windows, and the Claude Code and Codex sessions in them, once xaya runs again.
//!
//! What's open is saved as a snapshot while xaya runs. Windows you close leave it; windows still
//! open when xaya stops, because it quit, crashed or was updated, are opened again in their
//! directory on the next start, resuming the agent session that ran in them.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::session::RestoreMode;

/// Snapshot format version; snapshots of other versions are ignored.
const VERSION: u32 = 1;

/// Left beside the snapshot once restoring agents was explained, see [`claim_restore_notice`].
const RESTORE_NOTICE_MARKER: &str = "restore-notice-shown";

/// Day directories of Codex sessions searched for a window's session, newest first.
const CODEX_DAYS: usize = 7;

/// The windows open when the snapshot was saved.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u32,
    pub windows: Vec<SessionWindow>,
}

/// One window to open again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionWindow {
    /// The foreground program's working directory.
    pub cwd: Option<PathBuf>,
    /// The agent running in the window.
    pub agent: Option<AgentSession>,
    /// Label shown in front of the window's title, such as a worktree's branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// An agent session to resume.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "app", rename_all = "kebab-case")]
pub enum AgentSession {
    ClaudeCode {
        session_id: String,
    },
    /// Codex doesn't say which session a process runs, so it's guessed from the directory; when
    /// that's ambiguous, Codex asks which one to resume.
    Codex {
        session_id: Option<String>,
    },
}

impl AgentSession {
    /// The command resuming this session in its directory.
    pub fn resume_command(&self) -> String {
        match self {
            Self::ClaudeCode { session_id } => format!("claude --resume {}", quote(session_id)),
            Self::Codex { session_id: Some(session_id) } => {
                let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("xaya"));
                format!(
                    "{} agent restore {}",
                    quote(&executable.to_string_lossy()),
                    quote(session_id)
                )
            },
            Self::Codex { session_id: None } => String::from("codex resume"),
        }
    }
}

impl Snapshot {
    pub fn new(windows: Vec<SessionWindow>) -> Self {
        Self { version: VERSION, windows }
    }

    /// The windows to open again under `mode`, dropping directories that no longer exist.
    pub fn restorable(self, mode: RestoreMode) -> Vec<SessionWindow> {
        if self.version != VERSION {
            return Vec::new();
        }
        self.windows
            .into_iter()
            .filter(|window| match mode {
                RestoreMode::Agents => window.agent.is_some(),
                RestoreMode::All => true,
                RestoreMode::Never => false,
            })
            .map(|mut window| {
                if !window.cwd.as_ref().is_some_and(|cwd| cwd.is_dir()) {
                    // Without its directory, a session can't be found again, nor its worktree.
                    window.cwd = None;
                    window.agent = None;
                    window.label = None;
                }
                window
            })
            .filter(|window| mode == RestoreMode::All || window.agent.is_some())
            .collect()
    }
}

/// Drop the Codex session of windows that share it, so each asks which to resume.
///
/// Codex sessions are found by directory, so two Codex windows in one directory find the same.
pub fn forget_shared_codex_sessions(windows: &mut [SessionWindow]) {
    let id = |window: &SessionWindow| match &window.agent {
        Some(AgentSession::Codex { session_id: Some(id) }) => Some(id.clone()),
        _ => None,
    };
    let ids: Vec<_> = windows.iter().filter_map(id).collect();
    for window in windows.iter_mut() {
        if id(window).is_some_and(|id| ids.iter().filter(|other| **other == id).count() > 1) {
            window.agent = Some(AgentSession::Codex { session_id: None });
        }
    }
}

/// Where the snapshot is kept: `$XDG_STATE_HOME/xaya/session.json`, by default under
/// `~/.local/state`.
pub fn snapshot_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home::home_dir().map(|home| home.join(".local/state")))?;
    Some(state.join("xaya/session.json"))
}

/// Whether resuming agents still has to be explained, which the first restore does once.
///
/// Leaves a marker beside the snapshot at `snapshot`, so later restores stay quiet. When the
/// marker can't be written the explanation is skipped, rather than shown on every start.
pub fn claim_restore_notice(snapshot: &Path) -> bool {
    let marker = snapshot.with_file_name(RESTORE_NOTICE_MARKER);
    File::create_new(marker).is_ok()
}

/// Write `snapshot` to `path`, replacing the previous one in a single step.
pub fn save(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("json.partial");
    let mut file = File::create(&partial)?;
    file.write_all(&serde_json::to_vec_pretty(snapshot)?)?;
    file.sync_all()?;
    fs::rename(partial, path)
}

/// The snapshot at `path`, if there is a readable one.
pub fn load(path: &Path) -> Option<Snapshot> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Where Claude Code and Codex keep their state.
#[derive(Debug, Clone)]
pub struct AgentDirs {
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl AgentDirs {
    /// `~/.claude` and `$CODEX_HOME`, by default `~/.codex`.
    pub fn new() -> Option<Self> {
        let home = home::home_dir()?;
        let codex =
            std::env::var_os("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
        Some(Self { claude: home.join(".claude"), codex })
    }

    /// The Claude Code session run by a process in group `pgrp`.
    ///
    /// Claude Code writes `sessions/<pid>.json` for each running process, naming its session.
    /// `process_group` tells a process's group, `None` once it's gone. A session without a
    /// transcript yet had nothing to resume.
    pub fn claude_session(
        &self,
        pgrp: i32,
        process_group: impl Fn(i32) -> Option<i32>,
    ) -> Option<AgentSession> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ClaudeProcess {
            pid: i32,
            session_id: String,
            cwd: PathBuf,
        }

        let entries = fs::read_dir(self.claude.join("sessions")).ok()?;
        let process = entries.filter_map(Result::ok).find_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                return None;
            }
            let process: ClaudeProcess = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            (process_group(process.pid) == Some(pgrp)).then_some(process)
        })?;

        let transcript = format!("{}.jsonl", process.session_id);
        let project = self.claude.join("projects").join(claude_project_dir(&process.cwd));
        let exists = project.join(&transcript).is_file()
            // The directory naming is Claude Code's; look everywhere if it changed.
            || fs::read_dir(self.claude.join("projects")).ok().is_some_and(|dirs| {
                dirs.filter_map(Result::ok).any(|dir| dir.path().join(&transcript).is_file())
            });
        exists.then_some(AgentSession::ClaudeCode { session_id: process.session_id })
    }

    /// The Codex session last active in `cwd`, from the rollouts Codex records per day.
    pub fn codex_session(&self, cwd: &Path) -> Option<String> {
        #[derive(Deserialize)]
        struct Line {
            payload: Meta,
        }
        #[derive(Deserialize)]
        struct Meta {
            id: String,
            cwd: PathBuf,
        }

        // Newest first: the session in `cwd` is usually among the latest few.
        let mut rollouts: Vec<(SystemTime, PathBuf)> =
            newest_dirs(&self.codex.join("sessions"), 3, CODEX_DAYS)
                .iter()
                .flat_map(|day| fs::read_dir(day).into_iter().flatten().filter_map(Result::ok))
                .filter(|entry| {
                    entry.path().extension().is_some_and(|extension| extension == "jsonl")
                })
                .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
                .collect();
        rollouts.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

        rollouts.into_iter().find_map(|(_, path)| {
            // The first line describes the session.
            let mut first = String::new();
            BufReader::new(File::open(&path).ok()?).read_line(&mut first).ok()?;
            let line: Line = serde_json::from_str(&first).ok()?;
            (line.payload.cwd == cwd).then_some(line.payload.id)
        })
    }
}

/// The name of process `pid`'s executable, such as `codex`.
pub fn process_name(pid: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: `proc_name` writes at most `buffer.len()` bytes into the buffer.
        let len = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
        Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let name = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(name.trim_end().to_owned())
    }
}

/// The process group of process `pid`, `None` once it's gone.
pub fn process_group(pid: i32) -> Option<i32> {
    // SAFETY: `getpgid` only reads the process table.
    let pgrp = unsafe { libc::getpgid(pid) };
    (pgrp >= 0).then_some(pgrp)
}

/// Claude Code's directory name for a project: every character but letters and digits as `-`.
fn claude_project_dir(cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The `count` last directories `depth` levels below `root` in name order, such as Codex's
/// `YYYY/MM/DD`, newest first.
fn newest_dirs(root: &Path, depth: usize, count: usize) -> Vec<PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next: Vec<_> = level
            .iter()
            .flat_map(|dir| fs::read_dir(dir).into_iter().flatten().filter_map(Result::ok))
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        next.sort();
        // Each level only needs as many as the result; the rest are older.
        let keep = next.len().saturating_sub(count);
        level = next.split_off(keep);
    }
    level.reverse();
    level
}

/// `text` quoted for a POSIX shell when it needs to be.
pub(crate) fn quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xaya-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restore_notice_is_claimed_once() {
        let snapshot = temp_dir("notice").join("session.json");
        assert!(claim_restore_notice(&snapshot));
        assert!(!claim_restore_notice(&snapshot));
        // Without a directory to remember it in, it isn't shown at all.
        assert!(!claim_restore_notice(&snapshot.with_file_name("gone/session.json")));
    }

    #[test]
    fn resume_commands() {
        let claude = AgentSession::ClaudeCode { session_id: "11dc8133-7862".into() };
        assert_eq!(claude.resume_command(), "claude --resume 11dc8133-7862");
        let codex = AgentSession::Codex { session_id: Some("01a1c82e".into()) };
        assert!(codex.resume_command().ends_with(" agent restore 01a1c82e"));
        assert_eq!(AgentSession::Codex { session_id: None }.resume_command(), "codex resume");
        assert_eq!(quote("it's; rm"), r"'it'\''s; rm'");
    }

    #[test]
    fn snapshots_round_trip_and_filter() {
        let dir = temp_dir("snapshot");
        let agent = AgentSession::ClaudeCode { session_id: "abc".into() };
        let snapshot = Snapshot::new(vec![
            SessionWindow {
                cwd: Some(dir.clone()),
                agent: Some(agent.clone()),
                label: Some("⎇ agent-1".into()),
            },
            SessionWindow { cwd: Some(dir.clone()), agent: None, label: None },
            SessionWindow {
                cwd: Some(dir.join("gone")),
                agent: Some(agent.clone()),
                label: Some("⎇ gone".into()),
            },
        ]);

        let path = dir.join("state/xaya/session.json");
        save(&path, &snapshot).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, snapshot);

        // Only agents, and only where their directory still exists.
        let agents = loaded.clone().restorable(RestoreMode::Agents);
        assert_eq!(agents, [SessionWindow {
            cwd: Some(dir.clone()),
            agent: Some(agent),
            label: Some("⎇ agent-1".into())
        }]);
        // Everything, the gone directory as a plain window.
        let all = loaded.clone().restorable(RestoreMode::All);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], SessionWindow { cwd: None, agent: None, label: None });
        assert!(loaded.restorable(RestoreMode::Never).is_empty());

        // Other versions and broken files restore nothing.
        let future = Snapshot { version: VERSION + 1, ..snapshot };
        assert!(future.restorable(RestoreMode::All).is_empty());
        fs::write(&path, "{").unwrap();
        assert_eq!(load(&path), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_sessions_are_found_by_process_group() {
        let dir = temp_dir("claude");
        let dirs = AgentDirs { claude: dir.join(".claude"), codex: dir.join(".codex") };
        let sessions = dirs.claude.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let cwd = Path::new("/Users/me/my_repo.v2");
        for (pid, session) in [(100, "first"), (200, "second"), (300, "fresh")] {
            let json = format!(
                r#"{{"pid":{pid},"sessionId":"{session}","cwd":"{}","status":"busy"}}"#,
                cwd.display()
            );
            fs::write(sessions.join(format!("{pid}.json")), json).unwrap();
        }
        fs::write(sessions.join("100.key"), "secret").unwrap();
        let project = dirs.claude.join("projects").join("-Users-me-my-repo-v2");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("first.jsonl"), "").unwrap();
        // A transcript filed under a directory named differently is found too.
        let other = dirs.claude.join("projects").join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("second.jsonl"), "").unwrap();

        // Process 200 runs as a child in group 150; 100 leads its own group; 300 has no
        // transcript yet.
        let group = |pid| match pid {
            100 => Some(100),
            200 => Some(150),
            300 => Some(300),
            _ => None,
        };
        let session = |pgrp| match dirs.claude_session(pgrp, group) {
            Some(AgentSession::ClaudeCode { session_id }) => Some(session_id),
            _ => None,
        };
        assert_eq!(session(100).as_deref(), Some("first"));
        assert_eq!(session(150).as_deref(), Some("second"));
        assert_eq!(session(300), None);
        assert_eq!(session(999), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_sessions_are_the_newest_in_the_directory() {
        let dir = temp_dir("codex");
        let dirs = AgentDirs { claude: dir.join(".claude"), codex: dir.join(".codex") };
        let rollout = |day: &str, id: &str, cwd: &str| {
            let day = dirs.codex.join("sessions").join(day);
            fs::create_dir_all(&day).unwrap();
            let meta = format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}","base_instructions":"..."}}}}"#
            );
            fs::write(day.join(format!("rollout-{id}.jsonl")), format!("{meta}\n{{\"more\":1}}\n"))
                .unwrap();
            // Distinct modification times, in the order written.
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        rollout("2026/11/09", "old", "/repo");
        rollout("2026/11/10", "other-dir", "/elsewhere");
        rollout("2026/11/11", "new", "/repo");
        rollout("2026/11/11", "newest-elsewhere", "/elsewhere");

        assert_eq!(dirs.codex_session(Path::new("/repo")).as_deref(), Some("new"));
        assert_eq!(
            dirs.codex_session(Path::new("/elsewhere")).as_deref(),
            Some("newest-elsewhere")
        );
        assert_eq!(dirs.codex_session(Path::new("/nowhere")), None);

        assert_eq!(newest_dirs(&dirs.codex.join("sessions"), 3, 2).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
