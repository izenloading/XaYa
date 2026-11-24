//! What each turn of an agent changes in its git checkout, so the user sees it without reading
//! back through the agent's output.
//!
//! A turn runs from the agent reporting it works (`OSC 7501`) until nothing in it works or waits
//! on the user any more. At both ends the pane's worker thread snapshots the working tree as a git
//! tree, and the difference of the two trees is the turn's. Snapshots go through a copy of the
//! checkout's index: what the user staged stays as it is, files git ignores don't count, and
//! unchanged files aren't read again.
//!
//! Anything else writing to the checkout during the turn counts as the agent's, and edits it made
//! before the first snapshot finished count as an earlier turn's.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;
use std::{fs, thread};

use log::debug;

use crate::session::quote;
use crate::window_status::TurnChanges;

/// Tells the main thread how turn `turn` ended: what it changed, `None` when nothing.
pub type Report = Box<dyn Fn(u64, Option<TurnChanges>) + Send>;

/// Where a pane is in its agent's turns.
#[derive(Default)]
pub struct TurnTracker {
    in_turn: bool,
    /// The current or last turn.
    turn: u64,
    /// The snapshots are taken on this thread, in order.
    worker: Option<Sender<Job>>,
    /// The last turn's changes, and when they were seen.
    last: Option<(TurnChanges, Instant)>,
}

/// A turn starting or ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Start,
    End,
}

enum Job {
    /// Snapshot the checkout containing `dir` before turn `turn`.
    Start { turn: u64, dir: PathBuf },
    /// Snapshot it again after turn `turn` and report what changed.
    End { turn: u64 },
    /// Answer once the jobs before are done.
    #[cfg(test)]
    Sync(Sender<()>),
}

impl TurnTracker {
    /// Note whether the pane's agent is in a turn, returning whether one started or ended.
    pub fn observe(&mut self, in_turn: bool) -> Option<Edge> {
        if in_turn == self.in_turn {
            return None;
        }
        self.in_turn = in_turn;
        if in_turn {
            self.turn += 1;
            Some(Edge::Start)
        } else {
            Some(Edge::End)
        }
    }

    /// Snapshot the checkout containing `dir` for the turn that just started, starting the worker
    /// with `report` from `make_report` if there's none yet.
    pub fn start(&mut self, dir: PathBuf, make_report: impl FnOnce() -> Report) {
        let worker = self.worker.get_or_insert_with(|| spawn(make_report()));
        let _ = worker.send(Job::Start { turn: self.turn, dir });
    }

    /// Report what the turn that just ended changed, once the worker has looked.
    pub fn end(&mut self) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(Job::End { turn: self.turn });
        }
    }

    /// Turn `turn` ended having made `changes`.
    pub fn finished(&mut self, turn: u64, changes: Option<TurnChanges>) {
        debug!("Turn {turn} changed {:?}", changes.as_ref().map(TurnChanges::label));
        self.last = changes.map(|changes| (changes, Instant::now()));
    }

    /// The last turn's changes, and when they were seen; `None` when it changed nothing.
    pub fn last(&self) -> Option<(&TurnChanges, Instant)> {
        self.last.as_ref().map(|(changes, at)| (changes, *at))
    }
}

/// The shell command showing `changes`, such as `git -C ~/repo diff 1a2b… 3c4d…`.
pub fn diff_command(changes: &TurnChanges) -> String {
    let root = quote(&changes.root.to_string_lossy());
    format!("git -C {root} diff {} {}", changes.before, changes.after)
}

/// Start the worker taking snapshots, until its tracker is gone.
fn spawn(report: Report) -> Sender<Job> {
    let (sender, jobs) = mpsc::channel();
    let spawned = thread::Builder::new().name("turn changes".into()).spawn(move || {
        work(jobs, report);
    });
    if let Err(err) = spawned {
        debug!("Unable to track turn changes: {err}");
    }
    sender
}

fn work(jobs: Receiver<Job>, report: Report) {
    let mut before: Option<(u64, Snapshot)> = None;
    for job in jobs {
        match job {
            Job::Start { turn, dir } => {
                before = match snapshot(&dir) {
                    Ok(snapshot) => Some((turn, snapshot)),
                    Err(err) => {
                        debug!("No turn changes for {}: {err}", dir.display());
                        None
                    },
                };
            },
            Job::End { turn } => {
                // Without a snapshot from this turn's start, there's nothing to compare.
                let Some((started, before)) = before.take() else { continue };
                if started != turn {
                    continue;
                }
                match snapshot(&before.root).and_then(|after| changes(&before, &after)) {
                    Ok(changes) => report(turn, (changes.files > 0).then_some(changes)),
                    Err(err) => debug!("No turn changes for {}: {err}", before.root.display()),
                }
            },
            #[cfg(test)]
            Job::Sync(done) => {
                let _ = done.send(());
            },
        }
    }
}

/// The working tree of a checkout at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The checkout's top-level directory.
    pub root: PathBuf,
    /// Its working tree as a git tree, untracked files included and ignored ones not.
    pub tree: String,
}

/// Snapshot the checkout containing `dir`.
pub fn snapshot(dir: &Path) -> Result<Snapshot, String> {
    let root = PathBuf::from(git(dir, None, ["rev-parse", "--show-toplevel"])?.trim());
    // Each worktree has an index of its own.
    let index = git(&root, None, ["rev-parse", "--path-format=absolute", "--git-path", "index"])?;

    // Starting from a copy of the index, git only reads files that changed since.
    let scratch = ScratchIndex::new();
    match fs::copy(index.trim(), &scratch.0) {
        Ok(_) => (),
        // A repository without commits or staged files has no index yet.
        Err(err) if err.kind() == ErrorKind::NotFound => (),
        Err(err) => return Err(format!("unable to copy the index: {err}")),
    }
    git(&root, Some(&scratch.0), ["add", "--all"])?;
    let tree = git(&root, Some(&scratch.0), ["write-tree"])?.trim().to_owned();
    Ok(Snapshot { root, tree })
}

/// What changed in the working tree from `before` to `after`.
pub fn changes(before: &Snapshot, after: &Snapshot) -> Result<TurnChanges, String> {
    let (files, insertions, deletions) = if before.tree == after.tree {
        (0, 0, 0)
    } else {
        let args = ["diff", "--numstat", "-z", "--no-renames", &before.tree, &after.tree];
        numstat(&git(&before.root, None, args)?)
    };
    Ok(TurnChanges {
        files,
        insertions,
        deletions,
        root: before.root.clone(),
        before: before.tree.clone(),
        after: after.tree.clone(),
    })
}

/// Files, insertions and deletions in `git diff --numstat -z --no-renames` output, whose
/// entries are `insertions TAB deletions TAB path NUL`, with `-` counts for binary files.
fn numstat(output: &str) -> (usize, u64, u64) {
    output.split('\0').filter(|entry| !entry.is_empty()).fold(
        (0, 0, 0),
        |(files, insertions, deletions), entry| {
            let mut counts = entry.splitn(3, '\t').map(|count| count.parse::<u64>().unwrap_or(0));
            let added = counts.next().unwrap_or(0);
            let removed = counts.next().unwrap_or(0);
            (files + 1, insertions + added, deletions + removed)
        },
    )
}

/// An index file of our own, removed once done with.
struct ScratchIndex(PathBuf);

impl ScratchIndex {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("xaya-turn-{}-{n}.index", std::process::id());
        Self(std::env::temp_dir().join(name))
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        // Left behind when git was killed while writing it.
        let _ = fs::remove_file(self.0.with_extension("index.lock"));
    }
}

/// Run git in `dir`, with `index` as its index, returning its output or its error message.
///
/// Git's optional locks stay off, so the user's own git commands never wait for these.
fn git<I, S>(dir: &Path, index: Option<&Path>, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(|err| format!("unable to run git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.trim().trim_start_matches("fatal: ").to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    /// A repository with one commit in a fresh directory, `None` without git.
    fn repo(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("xaya-turns-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let run = |args: &[&str]| git(&dir, None, args.iter().copied());
        run(&["init", "-q", "-b", "main"]).ok()?;
        run(&["add", "."]).unwrap();
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]).unwrap();
        Some(dir.canonicalize().unwrap())
    }

    fn status(repo: &Path) -> String {
        git(repo, None, ["status", "--porcelain"]).unwrap()
    }

    #[test]
    fn turns_start_and_end_with_the_agents_work() {
        let mut tracker = TurnTracker::default();
        assert_eq!(tracker.observe(false), None);
        assert_eq!(tracker.observe(true), Some(Edge::Start));
        // Waiting on the user mid-turn keeps the turn going.
        assert_eq!(tracker.observe(true), None);
        assert_eq!(tracker.observe(false), Some(Edge::End));
        assert_eq!(tracker.observe(false), None);
        assert_eq!(tracker.observe(true), Some(Edge::Start));
        assert_eq!(tracker.turn, 2);
    }

    #[test]
    fn numstat_counts_files_and_lines() {
        assert_eq!(numstat(""), (0, 0, 0));
        let output = concat!("3\t1\tsrc/main.rs\0", "-\t-\tlogo.png\0", "10\t0\tnew file.rs\0");
        assert_eq!(numstat(output), (3, 13, 1));
    }

    #[test]
    fn snapshots_count_what_changed_and_leave_the_index_alone() {
        let Some(repo) = repo("changes") else { return };
        // Something the user staged before the turn.
        fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        git(&repo, None, ["add", "staged.txt"]).unwrap();
        let staged = status(&repo);

        let before = snapshot(&repo.join("src")).unwrap();
        assert_eq!(before.root, repo);

        // The turn edits a file, adds an untracked one and builds into an ignored directory.
        fs::write(repo.join("src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        fs::create_dir_all(repo.join("target")).unwrap();
        fs::write(repo.join("target/out"), "binary\n").unwrap();
        let after = snapshot(&repo).unwrap();

        let changes = changes(&before, &after).unwrap();
        assert_eq!((changes.files, changes.insertions, changes.deletions), (2, 4, 1));
        assert_eq!(changes.label(), "2 files +4 -1");
        assert_eq!(changes.root, repo);

        // What the user staged is just as it was, and the new files are still untracked.
        let now = status(&repo);
        assert!(now.contains("A  staged.txt"), "{now}");
        assert!(now.contains("?? src/lib.rs"), "{now}");
        assert!(staged.contains("A  staged.txt"));

        // The trees diff with plain git, as the panel's command does.
        let diff = git(&repo, None, ["diff", "--stat", &changes.before, &changes.after]).unwrap();
        assert!(diff.contains("src/lib.rs") && !diff.contains("target"), "{diff}");

        // Nothing changed, nothing to show.
        let again = snapshot(&repo).unwrap();
        assert_eq!(super::changes(&after, &again).unwrap().files, 0);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn snapshots_outside_a_repository_fail() {
        let dir = std::env::temp_dir().join(format!("xaya-turns-none-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // The temporary directory may itself be in a checkout; only check git refused when not.
        if git(&dir, None, ["rev-parse", "--show-toplevel"]).is_err() {
            assert!(snapshot(&dir).is_err());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worker_reports_each_turn_in_order() {
        let Some(repo) = repo("worker") else { return };
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let make_report = {
            let reports = reports.clone();
            move || -> Report {
                Box::new(move |turn, changes: Option<TurnChanges>| {
                    reports.lock().unwrap().push((turn, changes.map(|changes| changes.files)));
                    let _ = done.send(());
                })
            }
        };

        let mut tracker = TurnTracker::default();
        let wait = || finished.recv_timeout(Duration::from_secs(30)).unwrap();

        // Wait for the worker to take the snapshots asked for so far.
        let sync = |tracker: &TurnTracker| {
            let (done, synced) = mpsc::channel();
            tracker.worker.as_ref().unwrap().send(Job::Sync(done)).unwrap();
            synced.recv_timeout(Duration::from_secs(30)).unwrap();
        };

        // A turn writing a file.
        assert_eq!(tracker.observe(true), Some(Edge::Start));
        tracker.start(repo.clone(), make_report);
        sync(&tracker);
        fs::write(repo.join("notes.md"), "a\nb\n").unwrap();
        assert_eq!(tracker.observe(false), Some(Edge::End));
        tracker.end();
        wait();

        // A quiet turn.
        tracker.observe(true);
        tracker.start(repo.clone(), || unreachable!("the worker is running"));
        tracker.observe(false);
        tracker.end();
        wait();

        assert_eq!(*reports.lock().unwrap(), [(1, Some(1)), (2, None)]);
        assert!(status(&repo).contains("?? notes.md"));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn diff_commands_quote_the_checkout() {
        let changes = TurnChanges {
            files: 1,
            insertions: 1,
            deletions: 0,
            root: PathBuf::from("/tmp/it's here"),
            before: "aaa".into(),
            after: "bbb".into(),
        };
        assert_eq!(diff_command(&changes), r"git -C '/tmp/it'\''s here' diff aaa bbb");
    }
}
