//! Squash previews, isolated integration checks and guarded local branch advancement.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use xaya_agent_proto::{self as proto, LandPreview, LandPreviewParams, LandRecheck, LandStatus};

use crate::checkpoints::{Checkpoints, Git};
use crate::checks::Checks;
use crate::sessions::Sessions;
use crate::storage::{Store, epoch};
use crate::{Error, Result};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
fn conflict(message: &str) -> Error {
    Error::Rpc { code: proto::CONFLICT, message: message.into() }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

mod transaction;

/// Marks a preview whose landing, resume or reconcile is running outside the records lock.
struct Flight {
    set: Arc<Mutex<HashSet<String>>>,
    id: String,
}
impl Drop for Flight {
    fn drop(&mut self) {
        lock(&self.set).remove(&self.id);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Build a new candidate.
    Create,
    /// Sample the preconditions of an existing candidate again.
    Recheck,
    /// Advance a clean target at its old version.
    Land,
    /// Converge a target left between its old version and the candidate.
    Resume,
}

struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Context {
    store: Arc<Store>,
    sessions: Arc<Sessions>,
    checks: Arc<Checks>,
}

pub(crate) struct Landings {
    store: Arc<Store>,
    checkpoints: Arc<Checkpoints>,
    context: Arc<Context>,
    records: Arc<Mutex<BTreeMap<String, LandPreview>>>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    busy: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
}

impl Landings {
    pub fn open(
        store: Arc<Store>,
        sessions: Arc<Sessions>,
        checkpoints: Arc<Checkpoints>,
        checks: Arc<Checks>,
    ) -> Result<Self> {
        let mut records = BTreeMap::new();
        for mut preview in store.load_previews()? {
            if matches!(
                preview.status,
                LandStatus::Creating | LandStatus::Verifying | LandStatus::Landing
            ) {
                if preview.status == LandStatus::Landing {
                    preview.landing_outcome = Some(proto::LandOutcome::Unknown);
                } else {
                    preview.integration_verified = false;
                }
                preview.status = LandStatus::Interrupted;
                preview.stale = true;
                preview.error = Some("service restarted; landing preview was not replayed".into());
                store.save_preview(&preview)?;
            }
            records.insert(preview.id.clone(), preview);
        }
        Ok(Self {
            context: Arc::new(Context { store: Arc::clone(&store), sessions, checks }),
            store,
            checkpoints,
            records: Arc::new(Mutex::new(records)),
            in_flight: Arc::default(),
            workers: Mutex::default(),
            busy: Arc::default(),
            stopping: Arc::default(),
        })
    }

    fn permit(&self) -> Result<Permit> {
        let mut count = self.busy.load(Ordering::Acquire);
        loop {
            if count >= 2 {
                return Err(conflict("landing inspection capacity reached"));
            }
            match self.busy.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => count = current,
            }
        }
        Ok(Permit(Arc::clone(&self.busy)))
    }

    /// Call while holding the records lock, so the status read and the claim are atomic.
    fn flight(&self, id: &str) -> Result<Flight> {
        if !lock(&self.in_flight).insert(id.into()) {
            return Err(conflict("a landing operation on this preview is in progress"));
        }
        Ok(Flight { set: Arc::clone(&self.in_flight), id: id.into() })
    }

    fn integrated_receipt(&self, preview: &LandPreview) -> Result<()> {
        let check = self.context.checks.get(
            preview
                .integration_check_id
                .as_deref()
                .ok_or(Error::Protocol("missing integrated receipt"))?,
        )?;
        if check.integration_preview_id.as_deref() != Some(preview.id.as_str())
            || check.status != proto::CheckStatus::Succeeded
            || check.stale
            || Some(&check.commit) != preview.candidate_commit.as_ref()
            || Some(&check.tree) != preview.candidate_tree.as_ref()
        {
            return Err(conflict("integrated verification does not match candidate"));
        }
        Ok(())
    }

    /// Persists an uncertain intent before any write, then publishes only a durable result.
    fn attempt(&self, mut preview: LandPreview, mode: Mode) -> Result<LandPreview> {
        match inspect(&mut preview, &self.context, &self.stopping, mode) {
            Ok(()) => {
                preview.status = LandStatus::Landed;
                preview.landing_outcome = Some(proto::LandOutcome::Applied);
                preview.stale = false;
                preview.error = None;
            },
            Err(error) => {
                preview.error = Some(error.to_string().chars().take(512).collect());
                let outcome =
                    sample_outcome(&preview, &self.stopping).unwrap_or(proto::LandOutcome::Unknown);
                apply_outcome(&mut preview, outcome);
            },
        }
        // Publish the final result only after it is durable. If storage fails, both this
        // process and a restarted service retain the uncertain Landing/Unknown intent;
        // retries must not expose success or replay the branch update.
        self.store.save_preview(&preview)?;
        lock(&self.records).insert(preview.id.clone(), preview.clone());
        Ok(preview)
    }

    /// Multiple distinct landed candidates require target selection before inferring inclusion.
    pub fn landed_commits(&self, checkpoint: &str) -> std::collections::BTreeSet<String> {
        lock(&self.records)
            .values()
            .filter(|p| p.checkpoint_id == checkpoint && p.status == LandStatus::Landed)
            .filter_map(|p| p.candidate_commit.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<LandPreview> {
        lock(&self.records).get(id).cloned().ok_or_else(|| Error::Rpc {
            code: proto::NOT_FOUND,
            message: "unknown landing preview".into(),
        })
    }

    pub fn preview(&self, params: LandPreviewParams) -> Result<LandPreview> {
        if params.command_id.is_empty()
            || params.command_id.len() > 64
            || params.command_id.chars().any(char::is_control)
        {
            return Err(Error::Rpc {
                code: proto::INVALID_PARAMS,
                message: "command_id must contain 1..64 bytes without control characters".into(),
            });
        }
        if [&params.checkpoint_id, &params.check_id]
            .iter()
            .any(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err(Error::Rpc {
                code: proto::INVALID_PARAMS,
                message: "checkpoint_id and check_id must contain 1..128 bytes without control \
                          characters"
                    .into(),
            });
        }
        let target_ref = target_ref(&params.target_branch)?;
        let mut workers = lock(&self.workers);
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let mut records = lock(&self.records);
        if let Some(preview) = records.values().find(|preview| {
            preview.checkpoint_id == params.checkpoint_id && preview.command_id == params.command_id
        }) {
            if preview.check_id != params.check_id || preview.target_ref != target_ref {
                return Err(conflict("command_id reused with different landing parameters"));
            }
            return Ok(preview.clone());
        }
        if records.len() >= 128 {
            return Err(conflict("landing preview capacity reached"));
        }
        let permit = self.permit()?;
        let cp = self.checkpoints.get(&params.checkpoint_id)?;
        if cp.status != proto::CheckpointStatus::Ready {
            return Err(conflict("checkpoint is not ready"));
        }
        let required = |value: Option<String>| {
            value.ok_or(Error::Protocol("checkpoint identity is incomplete"))
        };
        let mut preview = LandPreview {
            id: format!("preview-{}", epoch()?),
            checkpoint_id: cp.id,
            check_id: params.check_id,
            command_id: params.command_id,
            session_id: cp.session_id,
            source_root: required(cp.root)?,
            checkpoint_commit: required(cp.commit)?,
            checkpoint_tree: required(cp.tree)?,
            checkpoint_base: required(cp.base_commit)?,
            checkpoint_ref: required(cp.git_ref)?,
            check_project_hash: None,
            binding_required: false,
            target_ref,
            target_head: None,
            target_worktree: None,
            merge_base: None,
            candidate_tree: None,
            candidate_commit: None,
            candidate_ref: None,
            diff_stat: None,
            details: None,
            output_truncated: false,
            status: LandStatus::Creating,
            landing_outcome: None,
            stale: true,
            integration_check_id: None,
            integration_verified: false,
            created_at_ms: now(),
            rechecked_at_ms: None,
            error: None,
        };
        self.store.save_preview(&preview)?;
        records.insert(preview.id.clone(), preview.clone());
        let task = preview.clone();
        let context = Arc::clone(&self.context);
        let store = Arc::clone(&self.store);
        let task_records = Arc::clone(&self.records);
        let stopping = Arc::clone(&self.stopping);
        match thread::Builder::new().name("landing-preview".into()).spawn(move || {
            let _permit = permit;
            let mut preview = task;
            if let Err(error) = inspect(&mut preview, &context, &stopping, Mode::Create) {
                preview.status = if matches!(&error, Error::Rpc { code, .. } if *code == proto::CONFLICT) { LandStatus::Blocked } else { LandStatus::Failed };
                preview.error = Some(error.to_string().chars().take(512).collect());
                preview.stale = true;
            }
            if stopping.load(Ordering::Acquire) { preview.status = LandStatus::Interrupted; preview.stale = true; }
            if serde_json::to_vec(&preview).is_ok_and(|value| value.len() > proto::MAX_FRAME_BYTES - 1024) {
                preview.status = LandStatus::Failed;
                preview.stale = true;
                preview.details = None;
                preview.diff_stat = None;
                preview.error = Some("landing preview exceeds protocol size limit".into());
            }
            if let Err(error) = store.save_preview(&preview) {
                preview.status = LandStatus::Failed;
                preview.stale = true;
                preview.error = Some(format!("landing preview storage failed: {error}"));
            }
            lock(&task_records).insert(preview.id.clone(), preview);
        }) {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                preview.status = LandStatus::Failed;
                preview.error = Some(error.to_string());
                self.store.save_preview(&preview)?;
                records.insert(preview.id.clone(), preview.clone());
            },
        }
        Ok(preview)
    }

    pub fn recheck(&self, id: &str) -> Result<LandRecheck> {
        let mut preview = self.get(id)?;
        if preview.status != LandStatus::Ready || preview.stale {
            return Ok(LandRecheck {
                preview,
                valid: false,
                reason: Some(
                    "preview is not ready or was invalidated; create a new preview".into(),
                ),
            });
        }
        let _permit = self.permit()?;
        let result = inspect(&mut preview, &self.context, &self.stopping, Mode::Recheck);
        if let Err(error) = result {
            preview.stale = true;
            preview.integration_verified = false;
            preview.error = Some(error.to_string().chars().take(512).collect());
        }
        preview.rechecked_at_ms = Some(now());
        let mut records = lock(&self.records);
        let previous = records.get(id).ok_or(Error::Protocol("landing preview disappeared"))?;
        if previous.status != LandStatus::Ready {
            return Ok(LandRecheck {
                preview: previous.clone(),
                valid: false,
                reason: Some("landing operation changed while rechecking".into()),
            });
        }
        // Concurrent rechecks may only invalidate an old receipt, never revive it.
        if previous.stale {
            preview.stale = true;
            preview.error = previous.error.clone();
        }
        // A recheck started before verification must not erase its completed receipt.
        preview.integration_check_id = previous.integration_check_id.clone();
        preview.integration_verified = !preview.stale && previous.integration_verified;
        records.insert(id.into(), preview.clone());
        self.store.save_preview(&preview)?;
        Ok(LandRecheck { valid: !preview.stale, reason: preview.error.clone(), preview })
    }

    pub fn verify(&self, id: &str) -> Result<LandPreview> {
        let mut workers = lock(&self.workers);
        let mut records = lock(&self.records);
        let mut preview = records.get(id).cloned().ok_or_else(|| Error::Rpc {
            code: proto::NOT_FOUND,
            message: "unknown landing preview".into(),
        })?;
        if preview.status == LandStatus::Verifying || preview.integration_check_id.is_some() {
            return Ok(preview);
        }
        if preview.status != LandStatus::Ready || preview.stale {
            return Err(conflict("preview must be ready and current"));
        }
        let permit = self.permit()?;
        preview.status = LandStatus::Verifying;
        self.store.save_preview(&preview)?;
        records.insert(id.into(), preview.clone());
        let mut task = preview.clone();
        let context = Arc::clone(&self.context);
        let store = Arc::clone(&self.store);
        let task_records = Arc::clone(&self.records);
        let stopping = Arc::clone(&self.stopping);
        match thread::Builder::new().name("landing-integration".into()).spawn(move || {
            let _permit = permit;
            let outcome = (|| -> Result<()> {
                inspect(&mut task, &context, &stopping, Mode::Recheck)?;
                task.status = LandStatus::Verifying;
                let check = context.checks.integrate(&task, &stopping, |id| {
                    let mut live = lock(&task_records);
                    let mut receipt = task.clone();
                    receipt.integration_check_id = Some(id.into());
                    store.save_preview(&receipt)?;
                    live.insert(receipt.id.clone(), receipt);
                    Ok(())
                })?;
                task.integration_check_id = Some(check.id);
                if check.status != proto::CheckStatus::Succeeded || check.stale {
                    return Err(conflict(
                        "integrated verification did not succeed; inspect check logs",
                    ));
                }
                inspect(&mut task, &context, &stopping, Mode::Recheck)?;
                task.integration_verified = true;
                Ok(())
            })();
            if task.integration_check_id.is_none() {
                task.integration_check_id = lock(&task_records)
                    .get(&task.id)
                    .and_then(|record| record.integration_check_id.clone());
            }
            if let Err(error) = outcome {
                task.status = LandStatus::Failed;
                task.stale = true;
                task.integration_verified = false;
                task.error = Some(error.to_string().chars().take(512).collect());
            }
            if stopping.load(Ordering::Acquire) {
                task.status = LandStatus::Interrupted;
                task.stale = true;
                task.integration_verified = false;
            }
            if let Err(error) = store.save_preview(&task) {
                task.status = LandStatus::Failed;
                task.stale = true;
                task.integration_verified = false;
                task.error = Some(format!("integration receipt storage failed: {error}"));
            }
            lock(&task_records).insert(task.id.clone(), task);
        }) {
            Ok(worker) => {
                workers.retain(|worker| !worker.is_finished());
                workers.push(worker);
            },
            Err(error) => {
                preview.status = LandStatus::Failed;
                preview.stale = true;
                preview.error = Some(error.to_string());
                self.store.save_preview(&preview)?;
                records.insert(id.into(), preview.clone());
            },
        }
        Ok(preview)
    }

    pub fn run(&self, id: &str) -> Result<LandPreview> {
        let _permit = self.permit()?;
        let (preview, _flight) = {
            let mut records = lock(&self.records);
            let mut preview = records.get(id).cloned().ok_or_else(|| Error::Rpc {
                code: proto::NOT_FOUND,
                message: "unknown landing preview".into(),
            })?;
            // A retry queries the durable outcome, never repeats a branch update.
            if preview.status == LandStatus::Landed || preview.landing_outcome.is_some() {
                return Ok(preview);
            }
            if preview.status != LandStatus::Ready || preview.stale || !preview.integration_verified
            {
                return Err(conflict(
                    "landing requires successful integrated verification of a current preview",
                ));
            }
            self.integrated_receipt(&preview)?;
            let flight = self.flight(id)?;
            preview.status = LandStatus::Landing;
            preview.landing_outcome = Some(proto::LandOutcome::Unknown);
            self.store.save_preview(&preview)?;
            records.insert(id.into(), preview.clone());
            (preview, flight)
        };
        // Git work runs outside the records lock; other previews stay queryable meanwhile.
        self.attempt(preview, Mode::Land)
    }

    /// Converges an interrupted or refused attempt onto the same verified candidate when the
    /// target provably holds only the old version, the candidate, or a mix of the two.
    pub fn resume(&self, id: &str) -> Result<LandPreview> {
        let _permit = self.permit()?;
        let (preview, _flight) = {
            let mut records = lock(&self.records);
            let mut preview = records.get(id).cloned().ok_or_else(|| Error::Rpc {
                code: proto::NOT_FOUND,
                message: "unknown landing preview".into(),
            })?;
            // A confirmed result is historical. Later edits must not turn it into another attempt.
            if preview.status == LandStatus::Landed {
                return Ok(preview);
            }
            if preview.landing_outcome.is_none() {
                return Err(conflict("preview has no durable landing attempt to resume"));
            }
            self.integrated_receipt(&preview)?;
            let flight = self.flight(id)?;
            preview.status = LandStatus::Landing;
            preview.landing_outcome = Some(proto::LandOutcome::Unknown);
            preview.rechecked_at_ms = Some(now());
            self.store.save_preview(&preview)?;
            records.insert(id.into(), preview.clone());
            (preview, flight)
        };
        self.attempt(preview, Mode::Resume)
    }

    pub fn reconcile(&self, id: &str) -> Result<LandPreview> {
        let _permit = self.permit()?;
        let (mut preview, _flight) = {
            let records = lock(&self.records);
            let preview = records.get(id).cloned().ok_or_else(|| Error::Rpc {
                code: proto::NOT_FOUND,
                message: "unknown landing preview".into(),
            })?;
            if preview.landing_outcome.is_none() {
                return Err(conflict("preview has no durable landing attempt to reconcile"));
            }
            // A confirmed result is historical. Later edits must not turn it into another attempt.
            if preview.status == LandStatus::Landed {
                return Ok(preview);
            }
            let flight = self.flight(id)?;
            (preview, flight)
        };
        let _leases = landing_leases(&preview, &self.context, "reconcile", Mode::Resume)?;
        let outcome =
            sample_outcome(&preview, &self.stopping).unwrap_or(proto::LandOutcome::Unknown);
        apply_outcome(&mut preview, outcome);
        preview.rechecked_at_ms = Some(now());
        preview.error = match outcome {
            proto::LandOutcome::Applied => None,
            proto::LandOutcome::NotApplied => Some(
                "target is still at the old version; `xaya land resume` may apply the verified \
                 candidate, no operation was replayed"
                    .into(),
            ),
            proto::LandOutcome::NeedsAttention => Some(
                "target reference, index or checkout do not form a clean old/candidate state; \
                 `xaya land resume` converges a proven mix, otherwise inspect manually"
                    .into(),
            ),
            proto::LandOutcome::Unknown => Some(
                "cannot confirm target identity or state (a lock may remain); no operation was \
                 replayed"
                    .into(),
            ),
        };
        self.store.save_preview(&preview)?;
        lock(&self.records).insert(id.into(), preview.clone());
        Ok(preview)
    }

    pub fn active(&self) -> bool {
        self.busy.load(Ordering::Acquire) != 0
    }
}
impl Drop for Landings {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for worker in lock(&self.workers).drain(..) {
            let _ = worker.join();
        }
    }
}

fn apply_outcome(preview: &mut LandPreview, outcome: proto::LandOutcome) {
    preview.landing_outcome = Some(outcome);
    preview.status = match outcome {
        proto::LandOutcome::Applied => LandStatus::Landed,
        proto::LandOutcome::NotApplied => LandStatus::Blocked,
        proto::LandOutcome::NeedsAttention => LandStatus::NeedsAttention,
        proto::LandOutcome::Unknown => LandStatus::Interrupted,
    };
    preview.stale = outcome != proto::LandOutcome::Applied;
}

/// Read-only sampling: it never repairs an index, advances a ref or removes Git locks.
fn sample_outcome(preview: &LandPreview, stopping: &AtomicBool) -> Result<proto::LandOutcome> {
    let root = Path::new(&preview.source_root).canonicalize()?;
    if root != Path::new(&preview.source_root) {
        return Ok(proto::LandOutcome::Unknown);
    }
    let git = Git::new(&root, stopping);
    let head = branch_head(&git, &preview.target_ref)?;
    if target_worktree(&git, &preview.target_ref)? != preview.target_worktree {
        return Ok(proto::LandOutcome::Unknown);
    }
    let reference_lock = git.run(
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &format!("{}.lock", preview.target_ref),
        ],
        None,
    )?;
    match std::fs::symlink_metadata(reference_lock) {
        Ok(_) => return Ok(proto::LandOutcome::Unknown),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let old = preview.target_head.as_deref().ok_or(Error::Protocol("missing old target"))?;
    let candidate =
        preview.candidate_commit.as_deref().ok_or(Error::Protocol("missing candidate"))?;
    if head != old && head != candidate {
        return Ok(proto::LandOutcome::NeedsAttention);
    }
    if let Some(path) = &preview.target_worktree {
        same_repository(&git, path, stopping)?;
        let checkout = Git::new(Path::new(path), stopping);
        if checkout.run(&["symbolic-ref", "--quiet", "HEAD"], None)? != preview.target_ref {
            return Ok(proto::LandOutcome::Unknown);
        }
        for name in ["index", "HEAD"] {
            let lock_path = checkout.run(
                &["rev-parse", "--path-format=absolute", "--git-path", &format!("{name}.lock")],
                None,
            )?;
            match std::fs::symlink_metadata(lock_path) {
                Ok(_) => return Ok(proto::LandOutcome::Unknown),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        if clean_target(path, stopping).is_err() {
            return Ok(proto::LandOutcome::NeedsAttention);
        }
        let matches = checkout.output(
            &["diff-index", "--cached", "--quiet", "--no-ext-diff", "--no-textconv", &head, "--"],
            None,
        )?;
        if !matches.status.success() {
            return Ok(proto::LandOutcome::NeedsAttention);
        }
    }
    Ok(if head == candidate { proto::LandOutcome::Applied } else { proto::LandOutcome::NotApplied })
}

pub(crate) fn target_ref(branch: &str) -> Result<String> {
    if branch.is_empty()
        || branch.len() > 256
        || branch.chars().any(char::is_control)
        || branch.starts_with('-')
        || (branch.starts_with("refs/") && !branch.starts_with("refs/heads/"))
    {
        return Err(Error::Rpc {
            code: proto::INVALID_PARAMS,
            message: "target must name a local branch (<=256 bytes)".into(),
        });
    }
    Ok(if branch.starts_with("refs/heads/") {
        branch.into()
    } else {
        format!("refs/heads/{branch}")
    })
}

fn branch_head(git: &Git<'_>, reference: &str) -> Result<String> {
    git.run(&["check-ref-format", reference], None)?;
    let symbolic = git.output(&["symbolic-ref", "--quiet", reference], None)?;
    if symbolic.status.success() {
        return Err(conflict("symbolic target branches are not supported"));
    }
    if symbolic.status.code() != Some(1) {
        return Err(conflict("cannot confirm target branch identity"));
    }
    git.run(&["show-ref", "--verify", "--hash", reference], None)
}

fn target_worktree(git: &Git<'_>, reference: &str) -> Result<Option<String>> {
    let output = git.output(&["worktree", "list", "--porcelain", "-z"], None)?;
    if !output.status.success() {
        return Err(conflict("could not inspect worktree ownership"));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| Error::Protocol("worktree paths must be UTF-8"))?;
    let mut found = None;
    let mut path = None;
    let mut branch = None;
    let mut unavailable = false;
    for field in text.split('\0') {
        if field.is_empty() {
            if branch == Some(reference) {
                if unavailable || found.is_some() {
                    return Err(conflict(
                        "target worktree is locked, prunable, or checked out more than once",
                    ));
                }
                let path = Path::new(path.ok_or(Error::Protocol("missing worktree path"))?)
                    .canonicalize()?;
                if path.as_os_str().len() > 1024 {
                    return Err(conflict("target worktree path exceeds 1024 bytes"));
                }
                found = Some(
                    path.to_str()
                        .ok_or(Error::Protocol("worktree paths must be UTF-8"))?
                        .to_owned(),
                );
            }
            path = None;
            branch = None;
            unavailable = false;
        } else if let Some(value) = field.strip_prefix("worktree ") {
            path = Some(value);
        } else if let Some(value) = field.strip_prefix("branch ") {
            branch = Some(value);
        } else if field == "locked"
            || field.starts_with("locked ")
            || field == "prunable"
            || field.starts_with("prunable ")
        {
            unavailable = true;
        }
    }
    Ok(found)
}

fn no_operation(git: &Git<'_>) -> Result<()> {
    let directory = PathBuf::from(git.run(&["rev-parse", "--absolute-git-dir"], None)?);
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_LOG",
    ] {
        if directory.join(name).exists() {
            return Err(conflict("worktree has an unfinished Git operation"));
        }
    }
    Ok(())
}

fn same_repository(source: &Git<'_>, path: &str, stopping: &AtomicBool) -> Result<()> {
    let common = |git: &Git<'_>| -> Result<PathBuf> {
        Ok(PathBuf::from(
            git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?,
        )
        .canonicalize()?)
    };
    if common(source)? != common(&Git::new(Path::new(path), stopping))? {
        return Err(conflict("target checkout now belongs to another repository"));
    }
    Ok(())
}

/// Shape requirements of a checked-out target, independent of its file content.
fn target_structure(path: &str, stopping: &AtomicBool) -> Result<()> {
    let mut git = Git::new(Path::new(path), stopping);
    git.prepare_merge()?;
    if PathBuf::from(git.run(&["rev-parse", "--show-toplevel"], None)?).canonicalize()?
        != Path::new(path).canonicalize()?
    {
        return Err(conflict("target checkout root changed"));
    }
    no_operation(&git)?;
    for key in ["core.sparseCheckout", "core.splitIndex"] {
        // Typed lookup honours every boolean spelling (yes/on/1) and the last assignment.
        let value = git.output(&["config", "--type=bool", "--get", key], None)?;
        match value.status.code() {
            Some(0) if value.stdout.starts_with(b"false") => (),
            Some(1) => (),
            _ => return Err(conflict("target sparse checkout or split index is not supported")),
        }
    }
    // Streamed: tracked listings of large repositories exceed any buffered output bound.
    let flagged = |record: &[u8]| {
        record.first().is_some_and(|flag| *flag == b'S' || flag.is_ascii_lowercase())
    };
    if git.find_record(&["ls-files", "-v", "-z"], None, flagged)?.is_some() {
        return Err(conflict("target skip-worktree or assume-unchanged files are not supported"));
    }
    if git
        .find_record(&["ls-files", "--stage", "-z"], None, |r| r.starts_with(b"160000 "))?
        .is_some()
    {
        return Err(conflict("target submodules are not supported"));
    }
    Ok(())
}

fn clean_target(path: &str, stopping: &AtomicBool) -> Result<()> {
    target_structure(path, stopping)?;
    let mut git = Git::new(Path::new(path), stopping);
    git.prepare_merge()?;
    let dirty = git.find_record(
        &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"],
        None,
        |_| true,
    );
    if !matches!(dirty, Ok(None)) {
        return Err(conflict("target worktree is dirty (tracked or untracked files)"));
    }
    Ok(())
}

fn bounded(text: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(text);
    let mut end = text.len().min(2048);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].into(), end < text.len())
}

fn checked_source(
    preview: &mut LandPreview,
    context: &Context,
    stopping: &AtomicBool,
    creating: bool,
) -> Result<()> {
    let check = context.checks.get(&preview.check_id)?;
    if check.status != proto::CheckStatus::Succeeded
        || check.stale
        // Binding canaries are evidence about a check, never a verification of record.
        || check.canary.is_some()
        || check.checkpoint_id != preview.checkpoint_id
        || check.session_id != preview.session_id
        || check.root != preview.source_root
        || check.commit != preview.checkpoint_commit
        || check.tree != preview.checkpoint_tree
        || check.base_commit != preview.checkpoint_base
        || check.steps.iter().any(|step| step.status != Some(proto::CheckStatus::Succeeded))
    {
        return Err(conflict(
            "verification is not successful, current, and bound to this checkpoint",
        ));
    }
    let cwd = Path::new(&check.cwd).canonicalize()?;
    let actual_root =
        PathBuf::from(Git::new(&cwd, stopping).run(&["rev-parse", "--show-toplevel"], None)?)
            .canonicalize()?;
    if actual_root != Path::new(&preview.source_root) {
        return Err(conflict(
            "verification working directory no longer belongs to the checkpoint repository",
        ));
    }
    if !creating && check.project_hash != preview.check_project_hash {
        return Err(conflict("verification project identity changed"));
    }
    preview.check_project_hash = check.project_hash;
    // Sticky: once a preview was held to the policy, a later edit cannot relax it.
    preview.binding_required |=
        crate::projects::binding_required(&context.store, &preview.source_root, stopping).map_err(
            |error| conflict(&format!("project landing policy cannot be read: {error}")),
        )?;
    if preview.binding_required && check.binding.status != proto::BindingStatus::Proved {
        return Err(conflict(
            "project requires a proved verification binding; run `xaya check bind` first",
        ));
    }
    Ok(())
}

/// Freezes managed writes to the source and target checkouts. A resume or reconcile must
/// remain possible after the originating session was evicted, so it does not require it.
fn landing_leases<'a>(
    preview: &LandPreview,
    context: &'a Context,
    purpose: &str,
    mode: Mode,
) -> Result<Vec<crate::sessions::WorkspaceLease<'a>>> {
    let mut roots = vec![PathBuf::from(&preview.source_root)];
    if let Some(path) = &preview.target_worktree {
        roots.push(PathBuf::from(path));
    }
    roots.sort();
    roots.dedup();
    let session = (mode != Mode::Resume).then_some(preview.session_id.as_str());
    roots
        .iter()
        .enumerate()
        .map(|(index, root)| {
            context.sessions.freeze_workspace_for_landing(
                session,
                root,
                &format!("{purpose}-{}-{index}", preview.id),
                &preview.id,
            )
        })
        .collect()
}

fn inspect(
    preview: &mut LandPreview,
    context: &Context,
    stopping: &AtomicBool,
    mode: Mode,
) -> Result<()> {
    let creating = mode == Mode::Create;
    let root = Path::new(&preview.source_root).canonicalize()?;
    if root != Path::new(&preview.source_root) {
        return Err(conflict("checkpoint repository path changed"));
    }
    let mut git = Git::new(&root, stopping);
    git.prepare_merge()?;
    no_operation(&git)?;
    let head = branch_head(&git, &preview.target_ref)?;
    let worktree = target_worktree(&git, &preview.target_ref)?;
    if let Some(path) = &worktree {
        same_repository(&git, path, stopping)?;
    }
    if creating {
        preview.target_head = Some(head.clone());
        preview.target_worktree = worktree.clone();
    }
    let old = preview.target_head.clone().ok_or(Error::Protocol("missing old target"))?;
    // Only a resume may find the target already at its candidate.
    let expected =
        head == old || (mode == Mode::Resume && preview.candidate_commit.as_ref() == Some(&head));
    if !expected || preview.target_worktree != worktree {
        return Err(conflict("target branch or worktree ownership changed since preview"));
    }
    let _leases = landing_leases(preview, context, "inspect", mode)?;
    if branch_head(&git, &preview.target_ref)? != head
        || target_worktree(&git, &preview.target_ref)? != worktree
    {
        return Err(conflict("target changed while acquiring workspace gates"));
    }
    checked_source(preview, context, stopping, creating)?;
    if git.run(&["show-ref", "--verify", "--hash", &preview.checkpoint_ref], None)?
        != preview.checkpoint_commit
        || git.run(
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{tree}}", preview.checkpoint_commit),
            ],
            None,
        )? != preview.checkpoint_tree
    {
        return Err(conflict("checkpoint reference or tree changed"));
    }
    if let Some(path) = &worktree
        && mode != Mode::Resume
    {
        clean_target(path, stopping)?;
    }
    if creating {
        // A private bare repository has no repository/user/system merge drivers or filters.
        // New objects go to the source object store; only our private ref is published there.
        let common = PathBuf::from(
            git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?,
        )
        .canonicalize()?;
        let format = git.run(&["rev-parse", "--show-object-format"], None)?;
        if !matches!(format.as_str(), "sha1" | "sha256") {
            return Err(conflict("unsupported Git object format"));
        }
        let directory = tempfile::tempdir()?;
        git.run(
            &[
                "init",
                "--bare",
                "--quiet",
                "--template=",
                &format!("--object-format={format}"),
                directory
                    .path()
                    .to_str()
                    .ok_or(Error::Protocol("temporary Git path must be UTF-8"))?,
            ],
            None,
        )?;
        let isolated = Git::isolated(directory.path(), stopping, common.join("objects"));
        let bases =
            isolated.output(&["merge-base", "--all", &head, &preview.checkpoint_commit], None)?;
        if !bases.status.success() {
            return Err(conflict("could not confirm a common merge base"));
        }
        let bases: Vec<_> = std::str::from_utf8(&bases.stdout)
            .map_err(|_| Error::Protocol("invalid merge base"))?
            .lines()
            .collect();
        if bases.len() != 1 {
            return Err(conflict("landing requires exactly one common merge base"));
        }
        preview.merge_base = Some(bases[0].into());
        let output = isolated.output(
            &["merge-tree", "--write-tree", "--messages", &head, &preview.checkpoint_commit],
            None,
        )?;
        if output.status.code() == Some(1) {
            let (text, truncated) = bounded(&output.stdout);
            preview.details = Some(text);
            preview.output_truncated = truncated;
            preview.status = LandStatus::Conflict;
            preview.error = Some("squash preview has merge conflicts".into());
            return Ok(());
        }
        if !output.status.success() {
            return Err(Error::Rpc {
                code: proto::ENGINE_ERROR,
                message: format!(
                    "merge-tree --write-tree failed or is unsupported: {}",
                    bounded(&output.stderr).0
                ),
            });
        }
        let tree = std::str::from_utf8(&output.stdout)
            .map_err(|_| Error::Protocol("invalid merge-tree output"))?
            .lines()
            .next()
            .filter(|tree| {
                matches!(tree.len(), 40 | 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or(Error::Protocol("missing merge result tree"))?
            .to_owned();
        let target_tree = git.run(&["rev-parse", "--verify", &format!("{head}^{{tree}}")], None)?;
        if tree == target_tree {
            return Err(conflict("checkpoint introduces no changes to the target"));
        }
        let diff = isolated.output(
            &[
                "diff",
                "--stat=80,40,30",
                "--no-ext-diff",
                "--no-textconv",
                &target_tree,
                &tree,
                "--",
            ],
            None,
        )?;
        if !diff.status.success() {
            return Err(conflict("could not inspect candidate diff"));
        }
        let (stat, truncated) = bounded(&diff.stdout);
        preview.diff_stat = Some(stat);
        preview.output_truncated |= truncated;
        let message = format!(
            "XaYa squash landing preview\n\nXaYa-Session: {}\nXaYa-Checkpoint: {}",
            preview.session_id, preview.checkpoint_id
        );
        let commit = isolated.run(&["commit-tree", &tree, "-p", &head, "-m", &message], None)?;
        let reference = format!("refs/xaya/landing-previews/{}", preview.id);
        git.run(&["update-ref", &reference, &commit, ""], None)?;
        preview.candidate_tree = Some(tree);
        preview.candidate_commit = Some(commit);
        preview.candidate_ref = Some(reference);
    } else {
        let commit = preview
            .candidate_commit
            .as_ref()
            .ok_or(Error::Protocol("missing landing candidate"))?;
        let reference = preview
            .candidate_ref
            .as_ref()
            .ok_or(Error::Protocol("missing landing candidate reference"))?;
        if git.run(&["show-ref", "--verify", "--hash", reference], None)? != *commit
            || Some(git.run(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")], None)?)
                != preview.candidate_tree
            || git
                .run(&["rev-list", "--parents", "-n", "1", commit], None)?
                .split_whitespace()
                .collect::<Vec<_>>()
                != vec![commit.as_str(), old.as_str()]
        {
            return Err(conflict("landing candidate reference, tree or parent changed"));
        }
    }
    checked_source(preview, context, stopping, false)?;
    if branch_head(&git, &preview.target_ref)? != head
        || target_worktree(&git, &preview.target_ref)? != worktree
    {
        return Err(conflict("target changed while building or rechecking the preview"));
    }
    if let Some(path) = &worktree
        && mode != Mode::Resume
    {
        clean_target(path, stopping)?;
    }
    if matches!(mode, Mode::Land | Mode::Resume) {
        let commit =
            preview.candidate_commit.as_deref().ok_or(Error::Protocol("missing candidate"))?;
        if let Some(path) = &worktree {
            transaction::advance(
                &transaction::Advance {
                    path,
                    reference: &preview.target_ref,
                    old: &old,
                    new: commit,
                    preview_id: &preview.id,
                    resume: mode == Mode::Resume,
                },
                stopping,
            )?;
        } else if head == old {
            git.run(&["update-ref", "--no-deref", &preview.target_ref, commit, &old], None)?;
        }
    }
    preview.status = LandStatus::Ready;
    preview.stale = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;

    #[test]
    fn restart_interrupts_preview_receipts_without_replaying_git() {
        for status in [LandStatus::Creating, LandStatus::Verifying, LandStatus::Landing] {
            let temp = tempfile::tempdir().unwrap();
            let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
            let store = Store::open(&runtime, None).unwrap();
            let preview = LandPreview {
                id: "preview-interrupted".into(),
                checkpoint_id: "checkpoint".into(),
                check_id: "check".into(),
                command_id: "never-replay".into(),
                session_id: "expired".into(),
                source_root: "/missing/repository".into(),
                checkpoint_commit: "missing".into(),
                checkpoint_tree: "missing".into(),
                checkpoint_base: "missing".into(),
                checkpoint_ref: "missing".into(),
                check_project_hash: None,
                binding_required: false,
                target_ref: "refs/heads/main".into(),
                target_head: None,
                target_worktree: None,
                merge_base: None,
                candidate_tree: None,
                candidate_commit: None,
                candidate_ref: None,
                diff_stat: None,
                details: None,
                output_truncated: false,
                status,
                landing_outcome: None,
                stale: true,
                integration_check_id: None,
                integration_verified: false,
                created_at_ms: 0,
                rechecked_at_ms: None,
                error: None,
            };
            store.save_preview(&preview).unwrap();
            let sessions = Arc::new(Sessions::open(Arc::clone(&store)).unwrap());
            let checkpoints =
                Arc::new(Checkpoints::open(Arc::clone(&store), Arc::clone(&sessions)).unwrap());
            let checks = Arc::new(
                Checks::open(Arc::clone(&store), Arc::clone(&sessions), Arc::clone(&checkpoints))
                    .unwrap(),
            );
            let landings =
                Landings::open(Arc::clone(&store), sessions, checkpoints, checks).unwrap();
            assert_eq!(landings.get(&preview.id).unwrap().status, LandStatus::Interrupted);
            let duplicate = landings
                .preview(LandPreviewParams {
                    checkpoint_id: preview.checkpoint_id,
                    check_id: preview.check_id,
                    target_branch: "main".into(),
                    command_id: preview.command_id,
                })
                .unwrap();
            assert_eq!(duplicate.id, preview.id);
            assert_eq!(duplicate.status, LandStatus::Interrupted);
            assert_eq!(store.load_previews().unwrap()[0].status, LandStatus::Interrupted);
            assert!(!landings.active());
        }
    }

    #[test]
    fn diagnostic_truncation_preserves_utf8_and_branch_arguments_are_not_revision_expressions() {
        let (text, truncated) = bounded("中🙂".repeat(1000).as_bytes());
        assert!(truncated);
        assert!(text.len() <= 2048);
        assert!(text.is_char_boundary(text.len()));
        assert_eq!(target_ref("main").unwrap(), "refs/heads/main");
        for branch in ["", "--help", "refs/tags/main", "main\n"] {
            assert!(target_ref(branch).is_err());
        }
    }
}
