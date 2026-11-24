//! Explicit argv verification, bound to private checkpoints. Commands are never replayed.
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use xaya_agent_proto::{self as proto, Check, CheckLog, CheckRunParams, CheckStatus};

use crate::checkpoints::{Checkpoints, Process, current_tree};
use crate::sessions::Sessions;
use crate::storage::{Launch, Store, epoch};
use crate::{Error, Result};

mod binding;
mod cleanup;
pub(crate) use cleanup::{create_owned, remove_owned};

const LOG_LIMIT: usize = 128 * 1024;
/// Exact error for "the command ran on an unchanged tree and exited non-zero".
const COMMAND_FAILED: &str = "verification command failed";

type Registry = Mutex<BTreeMap<String, Arc<Entry>>>;

/// Checks that own a private checkout: integrated candidates and binding canaries.
fn owns_checkout(check: &Check) -> bool {
    check.integration_preview_id.is_some() || check.canary.is_some()
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}
fn rpc(code: i32, message: &str) -> Error {
    Error::Rpc { code, message: message.into() }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub check: Check,
    pub launch: Launch,
    log: String,
    #[serde(default)]
    integration_source: Option<String>,
    #[serde(default)]
    integration_token: Option<String>,
}
struct Entry {
    record: Mutex<Record>,
    cancelled: AtomicBool,
}

pub(crate) struct Checks {
    store: Arc<Store>,
    sessions: Arc<Sessions>,
    checkpoints: Arc<Checkpoints>,
    entries: Arc<Registry>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    stopping: Arc<AtomicBool>,
}

impl Checks {
    pub fn open(
        store: Arc<Store>,
        sessions: Arc<Sessions>,
        checkpoints: Arc<Checkpoints>,
    ) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for mut record in store.load_checks()? {
            if record.check.status == CheckStatus::Running {
                record.check.status = CheckStatus::Interrupted;
                record.check.stale = true;
                for step in &mut record.check.steps {
                    if step.status == Some(CheckStatus::Running) {
                        step.status = Some(CheckStatus::Interrupted);
                    }
                }
                record.check.error =
                    Some("service restarted; verification was not replayed".into());
                store.save_check(&record)?;
            }
            if record.check.binding.status == proto::BindingStatus::Running {
                record.check.binding.status = proto::BindingStatus::CannotCheck;
                record.check.binding.reason =
                    Some("service restarted; binding was not replayed".into());
                store.save_check(&record)?;
            }
            entries.insert(
                record.check.id.clone(),
                Arc::new(Entry { record: Mutex::new(record), cancelled: AtomicBool::new(false) }),
            );
        }
        let service = Self {
            store,
            sessions,
            checkpoints,
            entries: Arc::new(Mutex::new(entries)),
            workers: Mutex::default(),
            stopping: Arc::default(),
        };
        let ids = lock(&service.entries).keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let entry = service.entry(&id)?;
            let pending = lock(&entry.record).check.integration_cleanup_pending;
            if pending {
                let _ = service.cleanup(&id);
            }
        }
        Ok(service)
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>> {
        lock(&self.entries).get(id).cloned().ok_or_else(|| rpc(proto::NOT_FOUND, "unknown check"))
    }

    pub fn get(&self, id: &str) -> Result<Check> {
        let entry = self.entry(id)?;
        let check = lock(&entry.record).check.clone();
        if !owns_checkout(&check) && check.status != CheckStatus::Running && !check.stale {
            // Never hold the registry or record lock during Git I/O.
            let matches = current_tree(Path::new(&check.cwd), &self.stopping)
                .is_ok_and(|(head, tree)| head == check.base_commit && tree == check.tree);
            let config_matches = check.project_hash.as_ref().is_none_or(|hash| {
                crate::projects::get_with_cancel(&self.store, &check.root, &self.stopping)
                    .is_ok_and(|info| info.trusted && &info.config_hash == hash)
            });
            if !matches || !config_matches {
                let mut record = lock(&entry.record);
                record.check.stale = true;
                record.check.error =
                    Some("workspace changed or freshness could not be confirmed".into());
                self.store.save_check(&record)?;
            }
        }
        Ok(lock(&entry.record).check.clone())
    }

    pub fn log(&self, id: &str, offset: usize) -> Result<CheckLog> {
        let entry = self.entry(id)?;
        let record = lock(&entry.record);
        if offset > record.log.len() || !record.log.is_char_boundary(offset) {
            return Err(rpc(proto::INVALID_PARAMS, "invalid UTF-8 log offset"));
        }
        // Even all-control-character text stays below the 64 KiB RPC frame limit.
        let mut end = (offset + 8192).min(record.log.len());
        while !record.log.is_char_boundary(end) {
            end -= 1;
        }
        Ok(CheckLog {
            text: record.log[offset..end].into(),
            next_offset: end,
            complete: record.check.status != CheckStatus::Running && end == record.log.len(),
            truncated: record.check.log_truncated,
        })
    }

    pub fn cancel(&self, id: &str) -> Result<Check> {
        let entry = self.entry(id)?;
        let record = lock(&entry.record);
        if record.check.status == CheckStatus::Running {
            entry.cancelled.store(true, Ordering::Release);
        }
        Ok(record.check.clone())
    }

    pub fn project_info(&self, cwd: &str) -> Result<proto::ProjectInfo> {
        crate::projects::get(&self.store, cwd)
    }

    pub fn trust_project(&self, cwd: &str, hash: &str) -> Result<proto::ProjectInfo> {
        crate::projects::trust(&self.store, cwd, hash)
    }

    pub fn latest(&self, session_id: &str) -> Result<Option<Check>> {
        let id = lock(&self.entries)
            .values()
            .map(|entry| lock(&entry.record).check.clone())
            // Canary runs are binding evidence, never the session's verification of record.
            .filter(|check| check.session_id == session_id && check.canary.is_none())
            .max_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then_with(|| a.id.cmp(&b.id)))
            .map(|check| check.id);
        id.map(|id| self.get(&id)).transpose()
    }

    pub fn project(&self, params: proto::ProjectCheckParams) -> Result<Check> {
        // A duplicate is a query of its original receipt, even if the repository config changed.
        let duplicate = lock(&self.entries)
            .values()
            .map(|entry| lock(&entry.record).check.clone())
            .find(|check| {
                check.checkpoint_id == params.checkpoint_id && check.command_id == params.command_id
            });
        if let Some(check) = duplicate {
            if check.project_hash.as_deref() != Some(&params.config_hash) {
                return Err(rpc(
                    proto::CONFLICT,
                    "command_id reused with a different project plan",
                ));
            }
            return self.get(&check.id);
        }
        let cp = self.checkpoints.get(&params.checkpoint_id)?;
        let info = self.project_info(&cp.cwd)?;
        if !info.trusted || info.config_hash != params.config_hash {
            return Err(rpc(
                proto::CONFLICT,
                "project configuration is untrusted or changed; review and trust the exact hash",
            ));
        }
        let request = CheckRunParams {
            checkpoint_id: params.checkpoint_id,
            command_id: params.command_id,
            argv: info.steps[0].argv.clone(),
            timeout_secs: info.timeout_secs,
        };
        self.run_plan(request, Some(info))
    }

    pub fn run(&self, params: CheckRunParams) -> Result<Check> {
        self.run_plan(params, None)
    }

    fn run_plan(
        &self,
        params: CheckRunParams,
        project: Option<proto::ProjectInfo>,
    ) -> Result<Check> {
        validate(&params)?;
        let mut workers = lock(&self.workers);
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let mut entries = lock(&self.entries);
        let duplicate = entries.values().find_map(|entry| {
            let record = lock(&entry.record);
            let check = &record.check;
            (check.checkpoint_id == params.checkpoint_id && check.command_id == params.command_id)
                .then(|| check.clone())
        });
        if let Some(check) = duplicate {
            if check.argv != params.argv
                || check.timeout_secs != params.timeout_secs
                || check.project_hash != project.as_ref().map(|info| info.config_hash.clone())
            {
                return Err(rpc(
                    proto::CONFLICT,
                    "command_id reused with different verification parameters",
                ));
            }
            drop(entries);
            drop(workers);
            return self.get(&check.id);
        }
        if entries
            .values()
            .filter(|entry| lock(&entry.record).check.status == CheckStatus::Running)
            .count()
            >= 2
            || entries.len() >= 128
        {
            return Err(rpc(proto::CONFLICT, "verification capacity reached"));
        }
        let cp = self.checkpoints.get(&params.checkpoint_id)?;
        if cp.status != proto::CheckpointStatus::Ready {
            return Err(rpc(proto::NOT_READY, "checkpoint is not ready"));
        }
        if project.as_ref().is_some_and(|info| cp.root.as_deref() != Some(&info.root)) {
            return Err(rpc(proto::CONFLICT, "project root differs from checkpoint root"));
        }
        let steps = project
            .as_ref()
            .map(|info| info.steps.clone())
            .unwrap_or_else(|| {
                vec![proto::VerifyStep {
                    name: "command".into(),
                    argv: params.argv.clone(),
                    cwd: cp.cwd.clone(),
                    timeout_secs: params.timeout_secs,
                }]
            })
            .into_iter()
            .map(|step| proto::CheckStepResult {
                step,
                status: None,
                exit_code: None,
                duration_ms: None,
            })
            .collect();
        let check = Check {
            binding: proto::Binding::default(),
            canary: None,
            integration_cleanup_pending: false,
            integration_cleanup_error: None,
            integration_preview_id: None,
            project_hash: project.map(|info| info.config_hash),
            steps,
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            id: format!("check-{}", epoch()?),
            checkpoint_id: cp.id,
            command_id: params.command_id,
            session_id: cp.session_id,
            argv: params.argv,
            timeout_secs: params.timeout_secs,
            cwd: cp.cwd,
            root: cp.root.ok_or(Error::Protocol("checkpoint root missing"))?,
            base_commit: cp.base_commit.ok_or(Error::Protocol("checkpoint HEAD missing"))?,
            tree: cp.tree.ok_or(Error::Protocol("checkpoint tree missing"))?,
            commit: cp.commit.ok_or(Error::Protocol("checkpoint commit missing"))?,
            status: CheckStatus::Running,
            stale: true,
            exit_code: None,
            duration_ms: None,
            error: None,
            log_truncated: false,
        };
        if serde_json::to_vec(&check)?.len() > proto::MAX_FRAME_BYTES - 1024 {
            return Err(rpc(
                proto::INVALID_PARAMS,
                "verification receipt exceeds protocol size limit",
            ));
        }
        let record = Record {
            check: check.clone(),
            launch: Launch::Ended,
            log: String::new(),
            integration_source: None,
            integration_token: None,
        };
        // Durable receipt precedes dispatch. A failed write must not launch a command.
        self.store.save_check(&record)?;
        let entry =
            Arc::new(Entry { record: Mutex::new(record), cancelled: AtomicBool::new(false) });
        entries.insert(check.id.clone(), Arc::clone(&entry));
        let store = Arc::clone(&self.store);
        let sessions = Arc::clone(&self.sessions);
        let stopping = Arc::clone(&self.stopping);
        let task = Arc::clone(&entry);
        match thread::Builder::new().name("verification".into()).spawn(move || {
            let started = Instant::now();
            let result = execute(&task, &store, &sessions, &stopping);
            let mut record = lock(&task.record);
            let check = &mut record.check;
            match result {
                Ok(status) => check.status = status,
                Err(error) => {
                    check.status = CheckStatus::Failed;
                    check.error = Some(error.to_string().chars().take(512).collect());
                },
            }
            if stopping.load(Ordering::Acquire) {
                check.status = CheckStatus::Interrupted;
                check.stale = true;
            } else if task.cancelled.load(Ordering::Acquire) {
                check.status = CheckStatus::Cancelled;
                check.stale = true;
            }
            check.duration_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
            if let Err(error) = store.save_check(&record) {
                record.check.status = CheckStatus::Failed;
                record.check.stale = true;
                record.check.error = Some(format!("verification storage failed: {error}"));
            }
        }) {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                let mut record = lock(&entry.record);
                record.check.status = CheckStatus::Failed;
                record.check.error = Some(error.to_string());
                self.store.save_check(&record)?;
                return Ok(record.check.clone());
            },
        }
        Ok(check)
    }

    /// Called by the landing worker; shares the existing durable logs and cancellation path.
    pub fn integrate(
        &self,
        preview: &proto::LandPreview,
        stopping: &AtomicBool,
        publish: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Check> {
        let original = self.get(&preview.check_id)?;
        validate(&CheckRunParams {
            checkpoint_id: original.checkpoint_id.clone(),
            command_id: original.command_id.clone(),
            argv: original.argv.clone(),
            timeout_secs: original.timeout_secs,
        })?;
        let commit =
            preview.candidate_commit.as_ref().ok_or(Error::Protocol("missing candidate"))?;
        let tree =
            preview.candidate_tree.as_ref().ok_or(Error::Protocol("missing candidate tree"))?;
        let mut check = private_template(&self.store, &original, commit, tree)?;
        check.integration_preview_id = Some(preview.id.clone());
        check.command_id = format!("integration-{}", preview.id);
        let private =
            Private { store: &self.store, sessions: &self.sessions, entries: &self.entries };
        private.run(check, &preview.source_root, None, stopping, publish)
    }

    /// Prove (or refute) that a succeeded check's gate reads the tree it names. Runs only in
    /// private checkouts; the receipt is durable before any canary is dispatched.
    pub fn bind(&self, params: proto::CheckBindParams) -> Result<Check> {
        if params.command_id.is_empty()
            || params.command_id.len() > 64
            || params.command_id.chars().any(char::is_control)
        {
            return Err(rpc(proto::INVALID_PARAMS, "expected command_id (1..64 bytes)"));
        }
        if let Some(path) = &params.path {
            binding::validate_path(path)?;
        }
        let parent = self.get(&params.check_id)?;
        let entry = self.entry(&params.check_id)?;
        if parent.canary.is_some() {
            return Err(rpc(proto::INVALID_PARAMS, "a binding canary run cannot itself be bound"));
        }
        if parent.binding.command_id.as_deref() == Some(params.command_id.as_str()) {
            return Ok(parent);
        }
        if parent.status != CheckStatus::Succeeded
            || parent.stale
            || parent.steps.iter().any(|step| step.status != Some(CheckStatus::Succeeded))
        {
            return Err(rpc(proto::CONFLICT, "only a successful, current check can be bound"));
        }
        let source_root =
            lock(&entry.record).integration_source.clone().unwrap_or_else(|| parent.root.clone());
        // Git I/O happens before any lock is taken.
        let git = crate::checkpoints::Git::new(Path::new(&source_root), &self.stopping);
        let picks = match &params.path {
            Some(path) if binding::tracked(&git, &parent.commit, path)? => vec![path.clone()],
            Some(_) => {
                return Err(rpc(
                    proto::INVALID_PARAMS,
                    "binding path is not a tracked regular file in this check's tree",
                ));
            },
            None => binding::candidates(&git, &parent.base_commit, &parent.commit)?,
        };
        let mut workers = lock(&self.workers);
        {
            let mut record = lock(&entry.record);
            let current = &record.check.binding;
            if current.command_id.as_deref() == Some(params.command_id.as_str()) {
                return Ok(record.check.clone());
            }
            match current.status {
                proto::BindingStatus::Running => {
                    return Err(rpc(proto::CONFLICT, "binding is already running"));
                },
                proto::BindingStatus::Proved | proto::BindingStatus::Unbound => {
                    return Err(rpc(
                        proto::CONFLICT,
                        "binding is already settled for this tree; it cannot change",
                    ));
                },
                proto::BindingStatus::NotChecked | proto::BindingStatus::CannotCheck => {},
            }
            let previous = record.check.binding.clone();
            record.check.binding = proto::Binding {
                status: if picks.is_empty() {
                    proto::BindingStatus::CannotCheck
                } else {
                    proto::BindingStatus::Running
                },
                command_id: Some(params.command_id),
                mutated_paths: Vec::new(),
                runs: Vec::new(),
                reason: picks.is_empty().then(|| {
                    "no tracked non-test source file to corrupt; pass an explicit path".into()
                }),
            };
            // Durable receipt precedes dispatch. A failed write must not launch a canary.
            if let Err(error) = self.store.save_check(&record) {
                record.check.binding = previous;
                return Err(error);
            }
            if picks.is_empty() {
                return Ok(record.check.clone());
            }
        }
        let (store, sessions, entries, stopping) = (
            Arc::clone(&self.store),
            Arc::clone(&self.sessions),
            Arc::clone(&self.entries),
            Arc::clone(&self.stopping),
        );
        let task = Arc::clone(&entry);
        let spawned = thread::Builder::new().name("binding".into()).spawn(move || {
            let private = Private { store: &store, sessions: &sessions, entries: &entries };
            let (status, reason) =
                match private.prove(&task, &parent, &source_root, &picks, &stopping) {
                    Ok(outcome) => outcome,
                    Err(error) => (
                        proto::BindingStatus::CannotCheck,
                        format!("binding could not be settled: {error}")
                            .chars()
                            .take(512)
                            .collect(),
                    ),
                };
            let mut record = lock(&task.record);
            record.check.binding.status = status;
            record.check.binding.reason = Some(reason);
            if let Err(error) = store.save_check(&record) {
                record.check.binding.status = proto::BindingStatus::CannotCheck;
                record.check.binding.reason = Some(format!("binding storage failed: {error}"));
            }
        });
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                let mut record = lock(&entry.record);
                record.check.binding.status = proto::BindingStatus::CannotCheck;
                record.check.binding.reason = Some(error.to_string());
                self.store.save_check(&record)?;
            },
        }
        Ok(lock(&entry.record).check.clone())
    }

    pub fn cleanup(&self, id: &str) -> Result<Check> {
        cleanup_entry(&self.store, &*self.entry(id)?)
    }

    pub fn active(&self) -> bool {
        lock(&self.entries)
            .values()
            .any(|entry| lock(&entry.record).check.status == CheckStatus::Running)
    }
}
impl Drop for Checks {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for entry in lock(&self.entries).values() {
            entry.cancelled.store(true, Ordering::Release);
        }
        for worker in lock(&self.workers).drain(..) {
            let _ = worker.join();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// A fresh receipt that reruns `original`'s plan in a new owned private checkout of `commit`.
fn private_template(store: &Store, original: &Check, commit: &str, tree: &str) -> Result<Check> {
    let id = format!("check-{}", epoch()?);
    let root = store.integration_root()?.join(&id).join("checkout");
    let root_text = root.to_str().ok_or(Error::Protocol("integration path must be UTF-8"))?;
    // An integrated parent's own checkout is gone after cleanup; its recorded paths still map.
    let original_cwd = Path::new(&original.cwd)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(&original.cwd).to_path_buf());
    let relative = original_cwd
        .strip_prefix(&original.root)
        .map_err(|_| Error::Protocol("original cwd is outside source root"))?;
    let mut check = original.clone();
    check.id = id;
    check.binding = proto::Binding::default();
    check.canary = None;
    check.integration_cleanup_pending = true;
    check.integration_cleanup_error = None;
    check.integration_preview_id = None;
    check.cwd = root.join(relative).to_str().ok_or(Error::Protocol("invalid cwd"))?.into();
    check.root = root_text.into();
    check.commit = commit.into();
    check.base_commit = commit.into();
    check.tree = tree.into();
    check.status = CheckStatus::Running;
    check.stale = true;
    check.exit_code = None;
    check.duration_ms = None;
    check.error = None;
    check.log_truncated = false;
    check.created_at_ms = now_ms();
    if check.steps.is_empty() {
        if check.project_hash.is_some() {
            return Err(Error::Protocol("project receipt has no verification steps"));
        }
        check.steps.push(proto::CheckStepResult {
            step: proto::VerifyStep {
                name: "command".into(),
                argv: check.argv.clone(),
                cwd: check.cwd.clone(),
                timeout_secs: check.timeout_secs,
            },
            status: None,
            exit_code: None,
            duration_ms: None,
        });
    }
    for result in &mut check.steps {
        result.status = None;
        result.exit_code = None;
        result.duration_ms = None;
        if check.project_hash.is_none() {
            result.step.cwd = check.cwd.clone();
        }
    }
    Ok(check)
}

/// Check out `commit` of the repository at `source_root` into the empty directory `root`, as a
/// separate repository that borrows the source's objects through alternates: no worktree is
/// registered with the source, and none of its configuration, hooks or templates apply.
pub(crate) fn private_checkout(
    source_root: &Path,
    root: &Path,
    commit: &str,
    stopping: &AtomicBool,
) -> Result<()> {
    let git = crate::checkpoints::Git::new(source_root, stopping);
    let common = git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?;
    let format = git.run(&["rev-parse", "--show-object-format"], None)?;
    let root_text = root.to_str().ok_or(Error::Protocol("integration path must be UTF-8"))?;
    git.run(
        &["init", "--quiet", "--template=", &format!("--object-format={format}"), root_text],
        None,
    )?;
    std::fs::write(root.join(".git/objects/info/alternates"), format!("{common}/objects\n"))?;
    let isolated = crate::checkpoints::Git::isolated(root, stopping, root.join(".git/objects"));
    isolated.run(&["checkout", "--quiet", "--detach", commit], None)?;
    Ok(())
}

/// What a private-checkout run needs; borrowed so worker threads never own `Checks`.
struct Private<'a> {
    store: &'a Arc<Store>,
    sessions: &'a Arc<Sessions>,
    entries: &'a Arc<Registry>,
}

impl Private<'_> {
    /// Register, check out `check.commit` privately, optionally corrupt one path, then verify.
    fn run(
        &self,
        check: Check,
        source_root: &str,
        mutate: Option<&str>,
        stopping: &AtomicBool,
        publish: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Check> {
        let root = PathBuf::from(&check.root);
        let commit = check.commit.clone();
        let record = Record {
            check: check.clone(),
            launch: Launch::Ended,
            log: String::new(),
            integration_source: Some(source_root.into()),
            integration_token: Some(epoch()?),
        };
        let entry =
            Arc::new(Entry { record: Mutex::new(record), cancelled: AtomicBool::new(false) });
        {
            let mut entries = lock(self.entries);
            if entries.len() >= 128
                || entries
                    .values()
                    .filter(|e| lock(&e.record).check.status == CheckStatus::Running)
                    .count()
                    >= 2
            {
                return Err(rpc(proto::CONFLICT, "verification capacity reached"));
            }
            self.store.save_check(&lock(&entry.record))?;
            entries.insert(check.id.clone(), Arc::clone(&entry));
        }
        let started = Instant::now();
        let outcome = (|| {
            publish(&check.id)?;
            cleanup::create(self.store, &lock(&entry.record))?;
            private_checkout(Path::new(source_root), &root, &commit, stopping)?;
            if let Some(path) = mutate {
                binding::corrupt(&root, path)?;
                // The canary is the only difference; record that exact tree as the expectation.
                let (head, tree) = current_tree(&root, stopping)?;
                let mut record = lock(&entry.record);
                record.check.base_commit = head;
                record.check.tree = tree;
                self.store.save_check(&record)?;
            }
            execute(&entry, self.store, self.sessions, stopping)
        })();
        let mut record = lock(&entry.record);
        match outcome {
            Ok(status) => record.check.status = status,
            Err(error) => {
                record.check.status = CheckStatus::Failed;
                record.check.error = Some(error.to_string().chars().take(512).collect());
            },
        }
        if stopping.load(Ordering::Acquire) {
            record.check.status = CheckStatus::Interrupted;
        } else if entry.cancelled.load(Ordering::Acquire) {
            record.check.status = CheckStatus::Cancelled;
        }
        record.check.stale |= record.check.status != CheckStatus::Succeeded;
        record.check.duration_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if let Err(error) = self.store.save_check(&record) {
            record.check.status = CheckStatus::Failed;
            record.check.stale = true;
            record.check.error = Some(format!("private checkout receipt storage failed: {error}"));
            return Err(error);
        }
        drop(record);
        cleanup_entry(self.store, &entry)
    }

    /// Clean baseline first, then up to two corrupted candidates. Only a failure caused by the
    /// command on an otherwise identical tree counts as proof.
    fn prove(
        &self,
        parent: &Arc<Entry>,
        original: &Check,
        source_root: &str,
        picks: &[String],
        stopping: &AtomicBool,
    ) -> Result<(proto::BindingStatus, String)> {
        let publish = |id: &str, path: Option<&str>| -> Result<()> {
            let mut record = lock(&parent.record);
            record.check.binding.runs.push(id.into());
            record.check.binding.mutated_paths.extend(path.map(str::to_owned));
            self.store.save_check(&record)
        };
        let canary = |role, mutated_path: Option<&str>| -> Result<Check> {
            let mut check =
                private_template(self.store, original, &original.commit, &original.tree)?;
            check.command_id = format!("canary-{}", check.id);
            check.canary = Some(proto::Canary {
                parent_check_id: original.id.clone(),
                role,
                mutated_path: mutated_path.map(str::to_owned),
            });
            Ok(check)
        };
        let baseline = self.run(
            canary(proto::CanaryRole::Baseline, None)?,
            source_root,
            None,
            stopping,
            |id| publish(id, None),
        )?;
        if baseline.status != CheckStatus::Succeeded {
            return Ok((
                proto::BindingStatus::CannotCheck,
                format!(
                    "the gate did not pass in a clean private checkout of this tree ({:?}); it \
                     may depend on ignored, untracked or machine-local files",
                    baseline.status
                ),
            ));
        }
        for path in picks {
            let run = self.run(
                canary(proto::CanaryRole::Mutated, Some(path))?,
                source_root,
                Some(path),
                stopping,
                |id| publish(id, Some(path)),
            )?;
            match run.status {
                CheckStatus::Succeeded => continue,
                CheckStatus::Failed if run.error.as_deref() == Some(COMMAND_FAILED) => {
                    let code = run.exit_code.map_or_else(|| "a signal".into(), |c| c.to_string());
                    return Ok((
                        proto::BindingStatus::Proved,
                        format!(
                            "passed on the clean tree, failed (exit {code}) with {path} corrupted"
                        ),
                    ));
                },
                status => {
                    return Ok((
                        proto::BindingStatus::CannotCheck,
                        format!("canary run with {path} corrupted ended {status:?}; inconclusive"),
                    ));
                },
            }
        }
        Ok((
            proto::BindingStatus::Unbound,
            format!(
                "the gate still passed with {} corrupted; its exit code is not evidence about \
                 this tree",
                picks.join(", ")
            ),
        ))
    }
}

fn cleanup_entry(store: &Store, entry: &Entry) -> Result<Check> {
    let mut record = lock(&entry.record);
    if !owns_checkout(&record.check) {
        return Err(rpc(proto::INVALID_PARAMS, "only owned private checkouts can be cleaned"));
    }
    if record.check.status == CheckStatus::Running {
        return Err(rpc(proto::CONFLICT, "private checkout verification is still running"));
    }
    if !record.check.integration_cleanup_pending {
        return Ok(record.check.clone());
    }
    match cleanup::remove(store, &record) {
        Ok(()) => {
            record.check.integration_cleanup_pending = false;
            record.check.integration_cleanup_error = None;
        },
        Err(error) => {
            record.check.integration_cleanup_error =
                Some(error.to_string().chars().take(512).collect())
        },
    }
    store.save_check(&record)?;
    Ok(record.check.clone())
}

pub(crate) fn validate(params: &CheckRunParams) -> Result<()> {
    if params.command_id.is_empty()
        || params.command_id.len() > 64
        || params.command_id.chars().any(char::is_control)
        || params.argv.is_empty()
        || params.argv[0].is_empty()
        || params.argv.len() > 128
        || params.argv.iter().map(String::len).sum::<usize>() > 8192
        || params.argv.iter().any(|arg| arg.contains('\0'))
        || !(1..=3600).contains(&params.timeout_secs)
    {
        return Err(rpc(
            proto::INVALID_PARAMS,
            "expected command_id (1..64 bytes), argv (1..128 arguments, <=8192 bytes) and \
             timeout_secs (1..3600)",
        ));
    }
    Ok(())
}

fn execute(
    entry: &Arc<Entry>,
    store: &Store,
    sessions: &Sessions,
    stopping: &AtomicBool,
) -> Result<CheckStatus> {
    let check = lock(&entry.record).check.clone();
    let root = Path::new(&check.root).canonicalize()?;
    let cwd = Path::new(&check.cwd).canonicalize()?;
    if !cwd.starts_with(&root) {
        return Err(Error::Protocol("verification cwd is outside checkpoint root"));
    }
    let _lease = sessions.freeze_workspace(&check.session_id, &root, &check.id)?;
    let expected = (check.base_commit.clone(), check.tree.clone());
    if current_tree(&cwd, &entry.cancelled)? != expected {
        return Err(Error::Protocol("checkpoint is stale; command was not executed"));
    }
    let integration_source = lock(&entry.record).integration_source.clone();
    let config_root = integration_source.as_deref().unwrap_or(&check.root);
    let candidate_config = || -> Result<()> {
        if integration_source.is_some()
            && let Some(hash) = &check.project_hash
        {
            use sha2::{Digest, Sha256};
            let path = root.join(".xaya/project.toml");
            if std::fs::symlink_metadata(root.join(".xaya"))?.file_type().is_symlink()
                || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
                || format!("{:x}", Sha256::digest(std::fs::read(path)?)) != *hash
            {
                return Err(Error::Protocol(
                    "integrated project configuration differs from trusted plan",
                ));
            }
        }
        Ok(())
    };
    candidate_config()?;
    let deadline = Instant::now() + Duration::from_secs(check.timeout_secs);
    for (index, result) in check.steps.iter().enumerate() {
        if entry.cancelled.load(Ordering::Acquire) || stopping.load(Ordering::Acquire) {
            return Ok(CheckStatus::Cancelled);
        }
        if let Some(hash) = &check.project_hash {
            let info = crate::projects::get_with_cancel(store, config_root, &entry.cancelled)?;
            if !info.trusted || &info.config_hash != hash {
                return Err(Error::Protocol("project configuration changed or trust was revoked"));
            }
        }
        let step_cwd = if check.project_hash.is_some() {
            crate::projects::resolve_cwd(&root, &result.step.cwd)?
        } else {
            cwd.clone()
        };
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(result.step.timeout_secs));
        if budget.is_zero() {
            return Ok(CheckStatus::TimedOut);
        }
        {
            let mut record = lock(&entry.record);
            record.check.steps[index].status = Some(CheckStatus::Running);
            record.check.exit_code = None;
            if check.project_hash.is_some() {
                append_log(&mut record, &format!("\n--- {} ---\n", result.step.name));
            }
            store.save_check(&record)?;
        }
        let started = Instant::now();
        let outcome = execute_command(entry, store, stopping, &result.step.argv, &step_cwd, budget);
        {
            let mut record = lock(&entry.record);
            let exit_code = record.check.exit_code;
            let step = &mut record.check.steps[index];
            step.duration_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
            step.exit_code = exit_code;
            step.status = Some(match &outcome {
                Ok(status) => *status,
                Err(_) => CheckStatus::Failed,
            });
            store.save_check(&record)?;
        }
        let status = outcome?;
        if !matches!(status, CheckStatus::Succeeded | CheckStatus::Failed) {
            return Ok(status);
        }
        candidate_config()?;
        let matches = current_tree(&cwd, &entry.cancelled)? == expected;
        if !matches || status == CheckStatus::Failed {
            let mut record = lock(&entry.record);
            record.check.stale = !matches;
            if !matches {
                record.check.steps[index].status = Some(CheckStatus::Failed);
            }
            record.check.error = Some(
                if matches {
                    COMMAND_FAILED
                } else {
                    "verification changed the workspace; result is stale"
                }
                .into(),
            );
            return Ok(CheckStatus::Failed);
        }
        if let Some(hash) = &check.project_hash {
            let info = crate::projects::get_with_cancel(store, config_root, &entry.cancelled)?;
            if !info.trusted || &info.config_hash != hash {
                return Err(Error::Protocol("project configuration changed during verification"));
            }
        }
    }
    lock(&entry.record).check.stale = false;
    Ok(CheckStatus::Succeeded)
}

fn execute_command(
    entry: &Arc<Entry>,
    store: &Store,
    stopping: &AtomicBool,
    argv: &[String],
    cwd: &Path,
    budget: Duration,
) -> Result<CheckStatus> {
    let check = lock(&entry.record).check.clone();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .process_group(0)
        .env("XAYA_SESSION", &check.session_id)
        .env("XAYA_CHECKPOINT", &check.checkpoint_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    {
        let mut record = lock(&entry.record);
        record.launch = Launch::Prepared;
        store.save_check(&record)?;
    }
    let mut process = match command.spawn() {
        Ok(child) => Process(child, false),
        Err(error) => {
            lock(&entry.record).launch = Launch::Ended;
            return Err(error.into());
        },
    };
    let (out, err) = (
        process.0.stdout.take().ok_or(Error::Protocol("missing verification stdout"))?,
        process.0.stderr.take().ok_or(Error::Protocol("missing verification stderr"))?,
    );
    let output_done = Arc::new(AtomicBool::new(false));
    let readers = [
        reader(out, Arc::clone(entry), Arc::clone(&output_done)),
        reader(err, Arc::clone(entry), Arc::clone(&output_done)),
    ];
    let started = Instant::now();
    let result = (|| -> Result<CheckStatus> {
        let mut record = lock(&entry.record);
        record.launch = Launch::Running(process.0.id());
        store.save_check(&record)?;
        drop(record);
        let mut last_save = Instant::now();
        loop {
            if last_save.elapsed() >= Duration::from_millis(250) {
                store.save_check(&lock(&entry.record))?;
                last_save = Instant::now();
            }
            if stopping.load(Ordering::Acquire) {
                return Ok(CheckStatus::Interrupted);
            }
            if entry.cancelled.load(Ordering::Acquire) {
                return Ok(CheckStatus::Cancelled);
            }
            if started.elapsed() >= budget {
                return Ok(CheckStatus::TimedOut);
            }
            // Keep the leader unreaped until the whole group has been terminated.
            // SAFETY: valid zeroed output buffer and our own unreaped positive child PID.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    process.0.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if status != 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: waitid initialized the child-exit notification.
            if unsafe { info.si_pid() } != 0 {
                return Ok(CheckStatus::Succeeded);
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    process.kill_group();
    let status = process.0.wait();
    process.1 = status.is_ok();
    if status.is_ok() {
        lock(&entry.record).launch = Launch::Ended;
    }
    output_done.store(true, Ordering::Release);
    for reader in readers {
        reader.join().map_err(|_| Error::Protocol("verification output reader panicked"))??;
    }
    let result = result?;
    let status = status?;
    lock(&entry.record).check.exit_code = status.code();
    if result != CheckStatus::Succeeded {
        return Ok(result);
    }
    Ok(if status.success() { CheckStatus::Succeeded } else { CheckStatus::Failed })
}

fn append_log(record: &mut Record, text: &str) {
    if record.check.log_truncated {
        return;
    }
    let mut kept = (LOG_LIMIT - record.log.len()).min(text.len());
    while !text.is_char_boundary(kept) {
        kept -= 1;
    }
    record.log.push_str(&text[..kept]);
    record.check.log_truncated |= kept < text.len();
}

fn reader(
    mut source: impl Read + AsRawFd + Send + 'static,
    entry: Arc<Entry>,
    done: Arc<AtomicBool>,
) -> JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        // A descendant may leave the dedicated process group while retaining its pipes.
        // Nonblocking reads keep cleanup bounded even in that case.
        // SAFETY: source owns this live file descriptor for the entire reader lifetime.
        let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(source.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        let mut bytes = [0; 4096];
        let mut pending = Vec::new();
        let mut final_reads = 0;
        loop {
            if done.load(Ordering::Acquire) {
                final_reads += 1;
                if final_reads > 64 {
                    lock(&entry.record).check.log_truncated = true;
                    return Ok(());
                }
            }
            let len = match source.read(&mut bytes) {
                Ok(len) => len,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if done.load(Ordering::Acquire) {
                        0
                    } else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                },
                Err(error) => return Err(error),
            };
            pending.extend_from_slice(&bytes[..len]);
            // Preserve a Unicode codepoint split across pipe reads. Invalid bytes are replaced.
            let count = match std::str::from_utf8(&pending) {
                Err(error) if error.error_len().is_none() && len != 0 => error.valid_up_to(),
                _ => pending.len(),
            };
            let text = String::from_utf8_lossy(&pending[..count]);
            let mut record = lock(&entry.record);
            append_log(&mut record, &text);
            drop(record);
            pending.drain(..count);
            if len == 0 {
                return Ok(());
            }
        }
    })
}
