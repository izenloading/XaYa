//! Converges a checked-out target branch onto a verified candidate.
//!
//! Prepared reference locks remain held while the checkout is updated. The same operation
//! performs a first landing (everything at the old version) and resumes an interrupted one:
//! it proves that every path holds either the old version or the candidate, then lets Git's
//! own two-tree fast-forward finish the remaining paths. Nothing is ever overwritten unless
//! its content is proven to belong to one of those two versions.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{branch_head, clean_target, conflict, target_structure, target_worktree};
use crate::checkpoints::{Git, Process};
use crate::{Error, Result};

/// Content of an index.lock created by a landing. A crash leaves it behind; only a resume of
/// the same preview may adopt it, and nothing else is ever inferred from a foreign lock.
pub(super) fn lock_token(preview_id: &str) -> String {
    format!("xaya-landing {preview_id}\n")
}

/// The private index lives next to the real one under a name derived from the preview, so a
/// resume can find and discard what a crashed attempt left behind.
pub(super) fn private_index_name(preview_id: &str) -> String {
    format!("index.xaya-{preview_id}")
}

struct RefTransaction<'a> {
    process: Process,
    input: Option<ChildStdin>,
    replies: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    stopping: &'a AtomicBool,
    deadline: Instant,
    committed: bool,
}
impl<'a> RefTransaction<'a> {
    fn prepare(git: &Git<'_>, command: &str, stopping: &'a AtomicBool) -> Result<Self> {
        let mut process =
            git.command(&["update-ref", "--stdin", "-m", "XaYa verified landing"], None);
        process.stdin(Stdio::piped()).stderr(Stdio::null());
        let mut process = Process(process.spawn()?, false);
        let input = process.0.stdin.take().ok_or(Error::Protocol("missing transaction stdin"))?;
        let output =
            process.0.stdout.take().ok_or(Error::Protocol("missing transaction stdout"))?;
        let (sender, replies) = mpsc::sync_channel(8);
        let reader = thread::spawn(move || {
            for line in BufReader::new(output.take(4096)).lines() {
                match line {
                    Ok(line) => {
                        if sender.try_send(line).is_err() {
                            break;
                        }
                    },
                    _ => break,
                }
            }
        });
        let mut transaction = Self {
            process,
            input: Some(input),
            replies,
            reader: Some(reader),
            stopping,
            deadline: Instant::now() + Duration::from_secs(30),
            committed: false,
        };
        transaction.exchange("start\n", "start: ok")?;
        // An explicit transaction aborts on EOF, so a crash of this service never commits it.
        transaction.exchange(&format!("{command}prepare\n"), "prepare: ok")?;
        Ok(transaction)
    }

    fn exchange(&mut self, request: &str, expected: &str) -> Result<()> {
        self.input
            .as_mut()
            .ok_or(Error::Protocol("transaction is closed"))?
            .write_all(request.as_bytes())?;
        loop {
            if self.stopping.load(Ordering::Acquire) || Instant::now() >= self.deadline {
                return Err(conflict("reference transaction interrupted or timed out"));
            }
            match self.replies.recv_timeout(Duration::from_millis(50)) {
                Ok(reply) if reply == expected => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                _ => {
                    return Err(conflict(
                        "reference transaction rejected; target/HEAD must be unchanged and \
                         unlocked",
                    ));
                },
            }
        }
    }

    fn commit(&mut self) -> Result<()> {
        // The checkout update may have used most of the preparation budget. Once `commit` is
        // sent, wait for Git's own answer instead of reporting a timeout it may contradict.
        self.deadline = Instant::now() + Duration::from_secs(10);
        self.exchange("commit\n", "commit: ok")?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for RefTransaction<'_> {
    fn drop(&mut self) {
        // Closing an uncommitted transaction aborts it. Kill/wait also bounds reader cleanup.
        if !self.committed
            && let Some(input) = &mut self.input
        {
            let _ = input.write_all(b"abort\n");
        }
        self.input.take();
        // Give Git time to release its own locks. Killing a prepared transaction immediately
        // would leave HEAD.lock / branch.lock behind even for an ordinary preflight rejection.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.process.0.try_wait() {
                Ok(Some(_)) => {
                    self.process.1 = true;
                    break;
                },
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        if !self.process.1 {
            self.process.kill_group();
            self.process.1 = self.process.0.wait().is_ok();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path.parent().ok_or(Error::Protocol("missing index directory"))?)?.sync_all()?;
    Ok(())
}

struct IndexLock {
    path: PathBuf,
    file: File,
    installed: bool,
}
impl IndexLock {
    /// Creates the real index.lock holding this landing's token. With `adopt`, an existing
    /// lock is taken over only when it is a plain, singly linked file holding that token.
    fn acquire(index: &Path, token: &str, adopt: bool) -> Result<Self> {
        let mut name = index.as_os_str().to_os_string();
        name.push(".lock");
        let path = PathBuf::from(name);
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path);
        match created {
            Ok(file) => {
                let mut lock = Self { path, file, installed: false };
                lock.file.write_all(token.as_bytes())?;
                lock.file.sync_all()?;
                sync_directory(index)?;
                Ok(lock)
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && adopt => {
                let foreign = || conflict("target index is locked by another Git operation");
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(&path)
                    .map_err(|_| foreign())?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.nlink() != 1 {
                    return Err(foreign());
                }
                let mut content = Vec::new();
                (&mut file).take(4096).read_to_end(&mut content)?;
                if content != token.as_bytes() {
                    return Err(foreign());
                }
                Ok(Self { path, file, installed: false })
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(conflict("target index is locked by another Git operation"))
            },
            Err(error) => Err(error.into()),
        }
    }

    fn owned(&self) -> Result<bool> {
        let owned = self.file.metadata()?;
        Ok(fs::symlink_metadata(&self.path)
            .is_ok_and(|current| owned.dev() == current.dev() && owned.ino() == current.ino()))
    }

    fn install(&mut self, from: &Path, index: &Path) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut File::open(from)?, &mut self.file)?;
        self.file.sync_all()?;
        if !self.owned()? {
            return Err(conflict("index lock ownership changed"));
        }
        fs::rename(&self.path, index)?;
        self.installed = true;
        sync_directory(index)
    }
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.installed && self.owned().unwrap_or(false) {
            let _ = fs::remove_file(&self.path);
            let _ = sync_directory(&self.path);
        }
    }
}

/// A copy of the real index used for all inspection and the checkout update.
struct PrivateIndex {
    path: PathBuf,
    lock: PathBuf,
}
impl PrivateIndex {
    fn create(index: &Path, preview_id: &str) -> Result<Self> {
        let path = index
            .parent()
            .ok_or(Error::Protocol("missing index directory"))?
            .join(private_index_name(preview_id));
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        // Git holds this lock while it writes the private index. After a crash of this
        // service, an orphaned writer may still be running; never race it.
        if fs::symlink_metadata(&lock).is_ok() {
            return Err(conflict(&format!(
                "a previous landing Git process may still be running; once none uses {}, remove \
                 it and resume again",
                lock.display()
            )));
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => fs::remove_file(&path)?,
            Ok(_) => return Err(conflict("unexpected node at the private landing index path")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let private = Self { path, lock };
        std::io::copy(&mut File::open(index)?, &mut copy)?;
        Ok(private)
    }
}
impl Drop for PrivateIndex {
    fn drop(&mut self) {
        // Every Git process this service started on the private index has been reaped, so a
        // remaining lock is our own killed writer's, never a live one's.
        let _ = fs::remove_file(&self.lock);
        let _ = fs::remove_file(&self.path);
    }
}

fn paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            std::str::from_utf8(p)
                .map(str::to_owned)
                .map_err(|_| conflict("landing paths must be UTF-8"))
        })
        .collect()
}

/// Parses `--name-status -z` output into (status letter, path) pairs.
fn statuses(bytes: &[u8]) -> Result<Vec<(u8, String)>> {
    let fields = paths(bytes)?;
    fields
        .chunks(2)
        .map(|pair| match pair {
            [status, path] => {
                Ok((status.as_bytes().first().copied().unwrap_or(b'?'), path.clone()))
            },
            _ => Err(Error::Protocol("truncated Git name-status output")),
        })
        .collect()
}

fn node(root: &Path, path: &str) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(root.join(path)) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        },
        Err(error) => Err(error.into()),
    }
}

struct Budget<'a> {
    stopping: &'a AtomicBool,
    deadline: Instant,
}
impl Budget<'_> {
    fn check(&self) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(conflict("target inspection interrupted or timed out"));
        }
        Ok(())
    }
}

fn index_holds(git: &Git<'_>, index: &Path, commit: &str) -> Result<bool> {
    let output = git.output(&["diff-index", "--cached", "--quiet", commit, "--"], Some(index))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(conflict("could not compare the target index")),
    }
}

/// Records the checkout's current content of every path the landing touches in `private`
/// and proves it is a mix of the old version and the candidate. Returns that mix as a tree.
fn classify(
    root: &Path,
    git: &Git<'_>,
    private: &Path,
    old: &str,
    new: &str,
    first_attempt: bool,
    budget: &Budget<'_>,
) -> Result<String> {
    let index_old = index_holds(git, private, old)?;
    if !index_old && (first_attempt || !index_holds(git, private, new)?) {
        return Err(conflict("target index is neither the old version nor the candidate"));
    }
    let changed = git.output(
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            old,
            new,
            "--",
        ],
        None,
    )?;
    if !changed.status.success() {
        return Err(conflict("could not list candidate paths"));
    }
    let mut add = Vec::new();
    let mut remove = Vec::new();
    let mut seen = HashSet::new();
    for path in paths(&changed.stdout)? {
        budget.check()?;
        match node(root, &path)? {
            Some(metadata) if !metadata.is_dir() => {
                // Case-insensitive or normalizing file systems map two spellings to one file.
                if !seen.insert((metadata.dev(), metadata.ino())) {
                    return Err(conflict(
                        "candidate paths differing only by case or Unicode normalization are not \
                         supported in a checked-out target",
                    ));
                }
                add.extend_from_slice(path.as_bytes());
                add.push(0);
            },
            _ => {
                remove.extend_from_slice(path.as_bytes());
                remove.push(0);
            },
        }
    }
    for (args, input) in [
        (&["update-index", "--force-remove", "-z", "--stdin"][..], remove),
        (&["update-index", "--add", "--replace", "-z", "--stdin"][..], add),
    ] {
        if input.is_empty() {
            continue;
        }
        let output = git.output_with_input(args, Some(private), Some(input))?;
        if !output.status.success() {
            return Err(conflict("could not record target checkout content"));
        }
    }
    let differs = |commit: &str| -> Result<Vec<(u8, String)>> {
        let output = git.output(
            &["diff-index", "--cached", "--name-status", "-z", "--no-renames", commit, "--"],
            Some(private),
        )?;
        if !output.status.success() {
            return Err(conflict("could not compare target checkout content"));
        }
        statuses(&output.stdout)
    };
    let from_old = differs(old)?;
    let from_new: HashSet<String> = differs(new)?.into_iter().map(|(_, path)| path).collect();
    for (status, path) in &from_old {
        if !from_new.contains(path) {
            continue;
        }
        // Present locally but in neither version: an untracked or ignored local file.
        return Err(if *status == b'A' {
            conflict(&format!("candidate would collide with a local ignored file: {path}"))
        } else {
            conflict(&format!("target file is neither the old version nor the candidate: {path}"))
        });
    }
    if first_attempt && !from_old.is_empty() {
        return Err(conflict("target checkout changed before the update"));
    }
    let _ = git.output(&["update-index", "-q", "--refresh"], Some(private))?;
    let modified = git.output(&["diff-files", "--name-only", "-z", "--"], Some(private))?;
    let untracked =
        git.output(&["ls-files", "--others", "--exclude-standard", "-z"], Some(private))?;
    if !modified.status.success() || !untracked.status.success() {
        return Err(conflict("could not inspect target checkout"));
    }
    if !modified.stdout.is_empty() || !untracked.stdout.is_empty() {
        return Err(conflict("target worktree is dirty (tracked or untracked files)"));
    }
    git.run(&["write-tree"], Some(private))
}

/// Every non-directory node below `directory` must be a tracked file the update deletes.
fn holds_only(
    root: &Path,
    directory: &str,
    deleted: &HashSet<String>,
    budget: &Budget<'_>,
) -> Result<bool> {
    let mut pending = vec![directory.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(root.join(&current))? {
            budget.check()?;
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| conflict("landing paths must be UTF-8"))?;
            let path = format!("{current}/{name}");
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if !deleted.contains(&path) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Paths the update creates must not exist locally, except as files it removes itself. The
/// file system lookup itself applies its case folding and Unicode normalization, so this cost
/// is proportional to the change, independent of ignored build trees.
fn collisions(root: &Path, git: &Git<'_>, mid: &str, new: &str, budget: &Budget<'_>) -> Result<()> {
    let output =
        git.output(&["diff-tree", "-r", "-z", "--no-renames", "--name-status", mid, new], None)?;
    if !output.status.success() {
        return Err(conflict("could not list remaining candidate paths"));
    }
    let changes = statuses(&output.stdout)?;
    let deleted: HashSet<String> =
        changes.iter().filter(|(s, _)| *s == b'D').map(|(_, p)| p.clone()).collect();
    let mut deleted_nodes = HashSet::new();
    for path in &deleted {
        budget.check()?;
        if let Some(metadata) = node(root, path)? {
            deleted_nodes.insert((metadata.dev(), metadata.ino()));
        }
    }
    let collision = |path: &str| {
        conflict(&format!("candidate would collide with a local ignored file or directory: {path}"))
    };
    for (_, path) in changes.iter().filter(|(s, _)| *s == b'A') {
        budget.check()?;
        match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) => {
                if deleted_nodes.contains(&(metadata.dev(), metadata.ino())) {
                    continue;
                }
                if !metadata.is_dir() || !holds_only(root, path, &deleted, budget)? {
                    return Err(collision(path));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                for ancestor in Path::new(path).ancestors().skip(1) {
                    let Some(name) = ancestor.to_str().filter(|name| !name.is_empty()) else {
                        continue;
                    };
                    if node(root, name)?.is_some_and(|m| !m.is_dir()) && !deleted.contains(name) {
                        return Err(collision(name));
                    }
                }
            },
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) struct Advance<'a> {
    pub path: &'a str,
    pub reference: &'a str,
    pub old: &'a str,
    pub new: &'a str,
    pub preview_id: &'a str,
    /// A first attempt requires the clean old state; a resume accepts any proven mix.
    pub resume: bool,
}

pub(super) fn advance(target: &Advance<'_>, stopping: &AtomicBool) -> Result<()> {
    let Advance { path, reference, old, new, preview_id, resume } = *target;
    let root = Path::new(path);
    let mut git = Git::new(root, stopping);
    git.prepare_merge()?;
    let head = branch_head(&git, reference)?;
    if head != old && !(resume && head == new) {
        return Err(conflict("target identity changed before checkout update"));
    }
    let command = if head == old {
        format!("update HEAD {new} {old}\n")
    } else {
        format!("verify HEAD {new}\n")
    };
    let mut transaction = RefTransaction::prepare(&git, &command, stopping)?;
    if branch_head(&git, reference)? != head
        || git.run(&["symbolic-ref", "--quiet", "HEAD"], None)? != reference
        || target_worktree(&git, reference)?.as_deref() != Some(path)
    {
        return Err(conflict("target identity changed before checkout update"));
    }
    target_structure(path, stopping)?;
    let index = PathBuf::from(
        git.run(&["rev-parse", "--path-format=absolute", "--git-path", "index"], None)?,
    );
    let metadata = fs::symlink_metadata(&index)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(conflict("target index must be a regular file without hard links"));
    }
    let mut index_lock = IndexLock::acquire(&index, &lock_token(preview_id), resume)?;
    let private = PrivateIndex::create(&index, preview_id)?;
    let budget = Budget { stopping, deadline: Instant::now() + Duration::from_secs(10) };
    let mid = classify(root, &git, &private.path, old, new, !resume, &budget)?;
    collisions(root, &git, &mid, new, &budget)?;
    if stopping.load(Ordering::Acquire) {
        return Err(conflict("landing was interrupted before checkout update"));
    }
    // No reset --hard or force checkout: Git's two-tree merge rejects unexpected local content.
    git.run(&["read-tree", "-m", "-u", &mid, new], Some(&private.path))?;
    let tree = git.run(&["write-tree"], Some(&private.path))?;
    if tree != git.run(&["rev-parse", &format!("{new}^{{tree}}")], None)? {
        return Err(conflict("updated index differs from candidate; inspect target checkout"));
    }
    index_lock.install(&private.path, &index)?;
    transaction.commit()?;
    clean_target(path, stopping)?;
    Ok(())
}
