//! Private Git snapshots. No checkout, branch update, or modification of the user's index.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use xaya_agent_proto::{self as proto, Checkpoint, CheckpointCreateParams, CheckpointStatus};

use crate::sessions::Sessions;
use crate::storage::{Store, epoch};
use crate::{Error, Result};

fn rpc(code: i32, message: &str) -> Error {
    Error::Rpc { code, message: message.into() }
}

fn record(id: &str, cwd: &str) -> Checkpoint {
    Checkpoint {
        id: id.into(),
        session_id: String::new(),
        command_id: String::new(),
        status: CheckpointStatus::Creating,
        cwd: cwd.into(),
        root: None,
        base_commit: None,
        tree: None,
        commit: None,
        git_ref: None,
        created_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        error: None,
    }
}

pub(crate) struct Checkpoints {
    store: Arc<Store>,
    sessions: Arc<Sessions>,
    records: Arc<Mutex<BTreeMap<String, Checkpoint>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    stopping: Arc<AtomicBool>,
}

impl Checkpoints {
    pub fn open(store: Arc<Store>, sessions: Arc<Sessions>) -> Result<Self> {
        let mut records = BTreeMap::new();
        for mut cp in store.load_checkpoints()? {
            if cp.status == CheckpointStatus::Creating {
                cp.status = CheckpointStatus::Interrupted;
                cp.error = Some("service restarted; checkpoint was not replayed".into());
                store.save_checkpoint(&cp)?;
            }
            records.insert(cp.id.clone(), cp);
        }
        Ok(Self {
            store,
            sessions,
            records: Arc::new(Mutex::new(records)),
            workers: Mutex::new(Vec::new()),
            stopping: Arc::default(),
        })
    }

    pub fn get(&self, id: &str) -> Result<Checkpoint> {
        self.records
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| rpc(proto::NOT_FOUND, "unknown checkpoint"))
    }

    pub fn create(&self, params: CheckpointCreateParams) -> Result<Checkpoint> {
        if params.command_id.is_empty()
            || params.command_id.len() > 64
            || params.command_id.chars().any(char::is_control)
        {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "command_id must contain 1..64 bytes without control characters",
            ));
        }
        let mut workers = self.workers.lock().unwrap();
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let mut records = self.records.lock().unwrap();
        if let Some(cp) = records
            .values()
            .find(|cp| cp.session_id == params.session_id && cp.command_id == params.command_id)
        {
            return Ok(cp.clone());
        }
        if workers.len() >= 2 || records.len() >= 128 {
            return Err(rpc(proto::CONFLICT, "checkpoint capacity reached"));
        }
        let session = self.sessions.get(&params.session_id)?;
        let mut cp = record(&format!("checkpoint-{}", epoch()?), &session.cwd);
        cp.session_id = params.session_id;
        cp.command_id = params.command_id;
        self.store.save_checkpoint(&cp)?;
        records.insert(cp.id.clone(), cp.clone());
        let task = cp.clone();
        let sessions = Arc::clone(&self.sessions);
        let store = Arc::clone(&self.store);
        let task_records = Arc::clone(&self.records);
        let stopping = Arc::clone(&self.stopping);
        let worker = thread::Builder::new().name("checkpoint".into()).spawn(move || {
            let mut cp = task;
            let result = (|| {
                let cwd = PathBuf::from(&cp.cwd);
                let git = Git::new(&cwd, &stopping);
                let root = PathBuf::from(git.run(&["rev-parse", "--show-toplevel"], None)?)
                    .canonicalize()?;
                let _lease = sessions.freeze_workspace(&cp.session_id, &root, &cp.id)?;
                capture(&git, &mut cp)
            })();
            match result {
                Ok(()) => cp.status = CheckpointStatus::Ready,
                Err(error) => {
                    cp.status = CheckpointStatus::Failed;
                    cp.error = Some(error.to_string().chars().take(512).collect());
                },
            }
            // A storage failure is visible in memory and cannot be reported as Ready.
            if let Err(error) = store.save_checkpoint(&cp) {
                cp.status = CheckpointStatus::Failed;
                cp.error = Some(format!("checkpoint storage failed: {error}"));
            }
            task_records.lock().unwrap().insert(cp.id.clone(), cp);
        });
        match worker {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                cp.status = CheckpointStatus::Failed;
                cp.error = Some(error.to_string());
                self.store.save_checkpoint(&cp)?;
                records.insert(cp.id.clone(), cp.clone());
            },
        }
        Ok(cp)
    }

    pub fn active(&self) -> bool {
        self.records.lock().unwrap().values().any(|cp| cp.status == CheckpointStatus::Creating)
    }
}

impl Drop for Checkpoints {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for worker in self.workers.get_mut().unwrap().drain(..) {
            let _ = worker.join();
        }
    }
}

pub(crate) struct Git<'a> {
    cwd: &'a Path,
    stopping: &'a AtomicBool,
    deadline: Instant,
    filters: Vec<String>,
    merge_drivers: Vec<String>,
    object_directory: Option<PathBuf>,
}

impl<'a> Git<'a> {
    pub(crate) fn new(cwd: &'a Path, stopping: &'a AtomicBool) -> Self {
        Self {
            cwd,
            stopping,
            deadline: Instant::now() + Duration::from_secs(30),
            filters: Vec::new(),
            merge_drivers: Vec::new(),
            object_directory: None,
        }
    }

    pub(crate) fn isolated(cwd: &'a Path, stopping: &'a AtomicBool, objects: PathBuf) -> Self {
        let mut git = Self::new(cwd, stopping);
        git.object_directory = Some(objects);
        git
    }

    pub(crate) fn prepare_merge(&mut self) -> Result<()> {
        let config = self.run(&["config", "--list"], None)?;
        for (key, _) in config.lines().filter_map(|line| line.split_once('=')) {
            if let Some(name) = key
                .strip_prefix("filter.")
                .and_then(|key| key.rsplit_once('.'))
                .map(|(name, _)| name)
            {
                self.filters.push(name.into());
            }
            if let Some(name) =
                key.strip_prefix("merge.").and_then(|key| key.strip_suffix(".driver"))
            {
                self.merge_drivers.push(name.into());
            }
        }
        Ok(())
    }

    pub(crate) fn run(&self, args: &[&str], index: Option<&Path>) -> Result<String> {
        let output = self.output(args, index)?;
        if !output.status.success() {
            return Err(rpc(
                proto::ENGINE_ERROR,
                &format!("Git failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
            ));
        }
        String::from_utf8(output.stdout)
            .map(|s| s.trim_end_matches('\n').into())
            .map_err(|_| Error::Protocol("Git paths must be UTF-8"))
    }

    pub(crate) fn command(&self, args: &[&str], index: Option<&Path>) -> Command {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        if let Some(objects) = &self.object_directory {
            command
                .env("GIT_OBJECT_DIRECTORY", objects)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_ATTR_NOSYSTEM", "1")
                .args(["-c", "core.attributesFile=/dev/null"]);
        }
        for filter in &self.filters {
            for setting in ["clean=", "smudge=", "process=", "required=false"] {
                command.arg("-c").arg(format!("filter.{filter}.{setting}"));
            }
        }
        for driver in &self.merge_drivers {
            command.arg("-c").arg(format!("merge.{driver}.driver=false"));
        }
        command
            .args([
                "--no-replace-objects",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "merge.renormalize=false",
                "-c",
                "rerere.enabled=false",
            ])
            .args(args)
            .current_dir(self.cwd)
            .process_group(0)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_AUTHOR_NAME", "XaYa")
            .env("GIT_AUTHOR_EMAIL", "checkpoint@xaya.invalid")
            .env("GIT_COMMITTER_NAME", "XaYa")
            .env("GIT_COMMITTER_EMAIL", "checkpoint@xaya.invalid")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
    }

    pub(crate) fn output(&self, args: &[&str], index: Option<&Path>) -> Result<GitOutput> {
        self.output_with_input(args, index, None)
    }

    /// Like `output`, feeding `input` on stdin from a separate thread so a full pipe in either
    /// direction cannot deadlock the bounded readers.
    pub(crate) fn output_with_input(
        &self,
        args: &[&str],
        index: Option<&Path>,
        input: Option<Vec<u8>>,
    ) -> Result<GitOutput> {
        let (status, stdout, stderr) = self.collect(args, index, input, bounded_read)?;
        Ok(GitOutput { status, stdout, stderr })
    }

    /// Streams NUL-terminated output records and returns the first one matching `wanted`,
    /// so listings of any size are inspected without buffering them. A matched record stops
    /// reading; Git's resulting exit status is then irrelevant.
    pub(crate) fn find_record(
        &self,
        args: &[&str],
        index: Option<&Path>,
        wanted: fn(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>> {
        let (status, found, stderr) = self.collect(args, index, None, move |stream| {
            let mut reader = io::BufReader::new(stream);
            let mut record = Vec::new();
            loop {
                record.clear();
                if io::BufRead::read_until(&mut reader, 0, &mut record)? == 0 {
                    return Ok(None);
                }
                if record.last() == Some(&0) {
                    record.pop();
                }
                if wanted(&record) {
                    return Ok(Some(record));
                }
            }
        })?;
        if found.is_none() && !status.success() {
            return Err(rpc(
                proto::ENGINE_ERROR,
                &format!("Git failed: {}", String::from_utf8_lossy(&stderr).trim()),
            ));
        }
        Ok(found)
    }

    fn collect<T: Send + 'static>(
        &self,
        args: &[&str],
        index: Option<&Path>,
        input: Option<Vec<u8>>,
        read: impl FnOnce(ChildStdout) -> io::Result<T> + Send + 'static,
    ) -> Result<(ExitStatus, T, Vec<u8>)> {
        let mut command = self.command(args, index);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        if self.stopping.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(Error::Protocol("checkpoint cancelled or exceeded 30 seconds"));
        }
        let mut process = Process(command.spawn()?, false);
        let writer = match input {
            Some(bytes) => {
                let mut stdin =
                    process.0.stdin.take().ok_or(Error::Protocol("missing Git input"))?;
                // A Git that exits early closes the pipe; its exit status reports the failure.
                Some(thread::spawn(move || {
                    let _ = stdin.write_all(&bytes);
                }))
            },
            None => None,
        };
        let out = process.0.stdout.take().ok_or(Error::Protocol("missing Git output"))?;
        let err = process.0.stderr.take().ok_or(Error::Protocol("missing Git error output"))?;
        let stdout = thread::spawn(move || read(out));
        let stderr = thread::spawn(move || bounded_read(err));
        let result = loop {
            if self.stopping.load(Ordering::Acquire) || Instant::now() >= self.deadline {
                break Err(Error::Protocol("checkpoint cancelled or exceeded 30 seconds"));
            }
            // Observe exit without reaping, so the group ID cannot be reused before cleanup.
            // SAFETY: an all-zero siginfo_t is a valid empty waitid output buffer.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: initialized siginfo pointer and our own positive, unreaped child PID.
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    process.0.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if status != 0 {
                break Err(io::Error::last_os_error().into());
            }
            // SAFETY: waitid succeeded; si_pid is valid for the child-exit notification.
            if unsafe { info.si_pid() } != 0 {
                break Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        };
        process.kill_group();
        let status = process.0.wait();
        process.1 = status.is_ok();
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let out = stdout.join().map_err(|_| Error::Protocol("Git reader failed"))??;
        let err = stderr.join().map_err(|_| Error::Protocol("Git reader failed"))??;
        result?;
        Ok((status?, out, err))
    }
}

pub(crate) struct GitOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) struct Process(pub(crate) Child, pub(crate) bool);
impl Process {
    pub(crate) fn kill_group(&mut self) {
        // SAFETY: this process created the dedicated group and has not yet reaped its leader.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Normal paths already reaped; never signal a potentially reused PID here.
        if !self.1 {
            self.kill_group();
            let _ = self.0.wait();
        }
    }
}

fn bounded_read(mut stream: impl Read) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    let mut overflow = false;
    loop {
        let length = stream.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        if result.len() + length <= 256 * 1024 {
            result.extend_from_slice(&buffer[..length]);
        } else {
            overflow = true;
        }
    }
    if overflow { Err(io::Error::other("Git output exceeds 256 KiB")) } else { Ok(result) }
}

fn snapshot(git: &Git<'_>) -> Result<(PathBuf, String, String)> {
    let root = PathBuf::from(git.run(&["rev-parse", "--show-toplevel"], None)?).canonicalize()?;
    let mut git = Git {
        cwd: &root,
        stopping: git.stopping,
        deadline: git.deadline,
        filters: Vec::new(),
        merge_drivers: Vec::new(),
        object_directory: None,
    };
    let head = git.run(&["rev-parse", "--verify", "HEAD^{commit}"], None)?;
    // Reject index modes whose references cannot safely be relocated to a temporary index.
    let settings = git.run(&["config", "--list"], None)?;
    if settings
        .lines()
        .any(|line| line == "core.sparsecheckout=true" || line == "core.splitindex=true")
    {
        return Err(Error::Protocol("checkpoint does not support sparse checkout or split index"));
    }
    // A checkpoint must never execute a repository-configured filter. Preserve materialized
    // working files rather than substituting LFS pointers or other external clean outputs.
    git.filters = settings
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(key, _)| {
            key.strip_prefix("filter.")?.rsplit_once('.').map(|(name, _)| name.to_owned())
        })
        .collect();
    let index_path =
        git.run(&["rev-parse", "--path-format=absolute", "--git-path", "index"], None)?;
    let temporary = tempfile::tempdir()?;
    let index = temporary.path().join("index");
    if Path::new(&index_path).exists() {
        fs::copy(&index_path, &index)?;
    } else {
        git.run(&["read-tree", "HEAD"], Some(&index))?;
    }
    let entries = git.run(&["ls-files", "--stage"], Some(&index))?;
    if entries.lines().any(|line| line.starts_with("160000 ")) {
        return Err(Error::Protocol("checkpoint does not support submodules"));
    }
    let flags = git.run(&["ls-files", "-v"], Some(&index))?;
    if flags.lines().any(|line| {
        line.starts_with('S') || line.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
    }) {
        return Err(Error::Protocol(
            "checkpoint does not support skip-worktree or assume-unchanged files",
        ));
    }
    git.run(&["add", "--all", "--", "."], Some(&index))?;
    if git
        .run(&["ls-files", "--stage"], Some(&index))?
        .lines()
        .any(|line| line.starts_with("160000 "))
    {
        return Err(Error::Protocol("checkpoint does not support embedded repositories"));
    }
    let tree = git.run(&["write-tree"], Some(&index))?;
    if git.run(&["rev-parse", "HEAD"], None)? != head {
        return Err(Error::Protocol("HEAD changed while capturing workspace"));
    }
    Ok((root, head, tree))
}

pub(crate) fn current_tree(cwd: &Path, stopping: &AtomicBool) -> Result<(String, String)> {
    let (_, head, tree) = snapshot(&Git::new(cwd, stopping))?;
    Ok((head, tree))
}

fn capture(git: &Git<'_>, cp: &mut Checkpoint) -> Result<()> {
    let (root, head, tree) = snapshot(git)?;
    let base_tree = git.run(&["rev-parse", "HEAD^{tree}"], None)?;
    if git.run(&["rev-parse", "HEAD"], None)? != head {
        return Err(Error::Protocol("HEAD changed while capturing checkpoint"));
    }
    let commit = if tree == base_tree {
        head.clone()
    } else {
        git.run(&["commit-tree", &tree, "-p", &head, "-m", "XaYa private checkpoint"], None)?
    };
    let reference = format!("refs/xaya/checkpoints/{}", cp.id);
    // Empty old value requires a new reference. Never overwrite an existing checkpoint.
    git.run(&["update-ref", &reference, &commit, ""], None)?;
    cp.root = Some(root.to_str().ok_or(Error::Protocol("repository path must be UTF-8"))?.into());
    cp.base_commit = Some(head);
    cp.tree = Some(tree);
    cp.commit = Some(commit);
    cp.git_ref = Some(reference);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.name", "Fixture"]);
        git(temp.path(), &["config", "user.email", "fixture@example.invalid"]);
        fs::write(temp.path().join("source"), "base\n").unwrap();
        fs::write(temp.path().join(".gitignore"), "generated\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "base"]);
        temp
    }

    #[test]
    fn checkpoint_preserves_head_index_and_ignored_files_and_survives_gc() {
        let temp = repo();
        let root = temp.path();
        fs::write(root.join("source"), "staged\n").unwrap();
        git(root, &["add", "source"]);
        fs::write(root.join("source"), "working\n").unwrap();
        fs::write(root.join("new"), "untracked\n").unwrap();
        fs::write(root.join("generated"), "ignored\n").unwrap();
        let head = git(root, &["rev-parse", "HEAD"]);
        let branch = git(root, &["symbolic-ref", "HEAD"]);
        let index = fs::read(root.join(".git/index")).unwrap();
        let stopped = AtomicBool::new(false);
        let mut cp = record("fixture", root.to_str().unwrap());
        let runner = Git::new(root, &stopped);
        capture(&runner, &mut cp).unwrap();
        assert_eq!(cp.base_commit.as_ref(), Some(&head));
        assert_ne!(cp.commit.as_ref(), Some(&head));
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(root, &["symbolic-ref", "HEAD"]), branch);
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        let commit = cp.commit.as_ref().unwrap();
        assert_eq!(git(root, &["show", &format!("{commit}:source")]), "working");
        assert_eq!(git(root, &["show", &format!("{commit}:new")]), "untracked");
        assert!(!git(root, &["ls-tree", "--name-only", commit]).lines().any(|p| p == "generated"));
        git(root, &["gc", "--prune=now"]);
        assert_eq!(git(root, &["rev-parse", cp.git_ref.as_ref().unwrap()]), *commit);
    }

    #[test]
    fn clean_checkpoint_reuses_head_and_unborn_head_is_rejected() {
        let temp = repo();
        let stopped = AtomicBool::new(false);
        let mut cp = record("clean", temp.path().to_str().unwrap());
        capture(&Git::new(temp.path(), &stopped), &mut cp).unwrap();
        assert_eq!(cp.commit, cp.base_commit);
        let empty = tempfile::tempdir().unwrap();
        git(empty.path(), &["init", "-q"]);
        let mut cp = record("unborn", empty.path().to_str().unwrap());
        assert!(capture(&Git::new(empty.path(), &stopped), &mut cp).is_err());
    }

    #[test]
    fn external_clean_filters_are_disabled_and_special_indexes_fail_closed() {
        let temp = repo();
        let root = temp.path();
        git(root, &["config", "filter.fixture.clean", "touch FILTER_RAN; cat"]);
        git(root, &["config", "filter.fixture.required", "true"]);
        fs::write(root.join(".gitattributes"), "source filter=fixture\n").unwrap();
        fs::write(root.join("source"), "materialized\n").unwrap();
        let stopped = AtomicBool::new(false);
        let mut cp = record("filter", root.to_str().unwrap());
        capture(&Git::new(root, &stopped), &mut cp).unwrap();
        assert!(!root.join("FILTER_RAN").exists());
        assert_eq!(git(root, &["show", &format!("{}:source", cp.commit.unwrap())]), "materialized");
        git(root, &["update-index", "--assume-unchanged", "source"]);
        let mut cp = record("unsupported", root.to_str().unwrap());
        assert!(capture(&Git::new(root, &stopped), &mut cp).is_err());
        assert!(cp.commit.is_none());
    }

    #[test]
    fn cancellation_and_deadline_return_without_creating_refs() {
        let temp = repo();
        let cancelled = AtomicBool::new(true);
        let mut cp = record("cancelled", temp.path().to_str().unwrap());
        assert!(capture(&Git::new(temp.path(), &cancelled), &mut cp).is_err());
        assert!(git(temp.path(), &["for-each-ref", "refs/xaya"]).is_empty());
        let stopped = AtomicBool::new(false);
        let runner = Git {
            cwd: temp.path(),
            stopping: &stopped,
            deadline: Instant::now(),
            filters: Vec::new(),
            merge_drivers: Vec::new(),
            object_directory: None,
        };
        assert!(capture(&runner, &mut cp).is_err());
    }

    #[test]
    fn running_git_and_its_pipe_holding_child_are_terminated_on_timeout() {
        let temp = repo();
        let stopped = AtomicBool::new(false);
        let started = Instant::now();
        let runner = Git {
            cwd: temp.path(),
            stopping: &stopped,
            deadline: started + Duration::from_secs(1),
            filters: Vec::new(),
            merge_drivers: Vec::new(),
            object_directory: None,
        };
        let error = runner
            .run(
                &["-c", "alias.checkpoint-wait=!printf started; exec sleep 60", "checkpoint-wait"],
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(5), "descendant kept Git pipes open");
    }

    #[test]
    fn restart_interrupts_checkpoint_receipts_without_replaying_git() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        let mut cp = record("checkpoint-interrupted", "/missing/repository");
        cp.session_id = "codex-expired".into();
        cp.command_id = "never-replay".into();
        store.save_checkpoint(&cp).unwrap();
        let sessions = Arc::new(Sessions::open(Arc::clone(&store)).unwrap());
        let checkpoints = Checkpoints::open(Arc::clone(&store), sessions).unwrap();
        assert_eq!(checkpoints.get(&cp.id).unwrap().status, CheckpointStatus::Interrupted);
        let duplicate = checkpoints
            .create(CheckpointCreateParams { session_id: cp.session_id, command_id: cp.command_id })
            .unwrap();
        assert_eq!(duplicate.id, cp.id);
        assert_eq!(store.load_checkpoints().unwrap()[0].status, CheckpointStatus::Interrupted);
        assert!(!checkpoints.active());
    }
}
