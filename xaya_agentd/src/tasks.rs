//! Tasks: a goal and testable acceptance criteria, attached to sessions. A session belongs to at
//! most one open task, so a review of its checkpoint never has to guess which criteria apply.
//! Criteria are fixed once created; their hashes bind the handoff bundles built from them.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use xaya_agent_proto::{
    self as proto, Attention, Criterion, Dispute, FixRound, LedgerEntry, LedgerStatus, Review,
    ReviewFinding, ReviewVerdict, Task, TaskCreateParams, TaskStatus,
};

use crate::sessions::Sessions;
use crate::storage::{Store, epoch};
use crate::{Error, Result};

pub(crate) mod handoff;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid(message: &str) -> Error {
    Error::Rpc { code: proto::INVALID_PARAMS, message: message.into() }
}

pub(crate) fn repository(cwd: &str, stopping: &std::sync::atomic::AtomicBool) -> Result<String> {
    use std::path::Path;
    let path = crate::checkpoints::Git::new(Path::new(cwd), stopping)
        .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?;
    Path::new(&path)
        .canonicalize()?
        .into_os_string()
        .into_string()
        .map_err(|_| invalid("repository path must be UTF-8"))
}

pub(crate) fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub(crate) struct Tasks {
    store: Arc<Store>,
    sessions: Arc<Sessions>,
    tasks: Mutex<BTreeMap<String, Task>>,
}

/// Text limited to `limit` characters, non-empty, with no control characters other than the
/// newlines and tabs `multiline` text may contain.
fn text(value: &str, what: &str, limit: usize, multiline: bool) -> Result<()> {
    let allowed = |c: char| !c.is_control() || (multiline && matches!(c, '\n' | '\t'));
    if value.trim().is_empty() || value.chars().count() > limit || !value.chars().all(allowed) {
        let lines = if multiline { "" } else { " on one line" };
        return Err(invalid(&format!(
            "{what} must be 1-{limit} characters{lines}, without control characters"
        )));
    }
    Ok(())
}

impl Tasks {
    pub fn open(store: Arc<Store>, sessions: Arc<Sessions>) -> Result<Self> {
        let tasks = store.load_tasks()?.into_iter().map(|task| (task.id.clone(), task)).collect();
        Ok(Self { store, sessions, tasks: Mutex::new(tasks) })
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn task_capacity(&self, project: &str) -> Result<usize> {
        self.store.task_capacity(project)
    }

    pub(crate) fn set_task_capacity(&self, project: &str, capacity: usize) -> Result<()> {
        self.store.set_task_capacity(project, capacity)
    }

    /// The project's default implementing engine, and whether it sets one.
    pub(crate) fn task_engine(&self, project: &str) -> Result<(proto::ReviewEngine, bool)> {
        Ok(match self.store.task_engine(project)? {
            Some(engine) => (engine, true),
            None => (proto::TASK_DEFAULT_ENGINE, false),
        })
    }

    pub(crate) fn set_task_engine(
        &self,
        project: &str,
        engine: Option<proto::ReviewEngine>,
    ) -> Result<()> {
        self.store.set_task_engine(project, engine)
    }

    /// The server validates engine paths and project ownership before taking the task lock.
    pub(crate) fn enqueue(&self, params: proto::TaskEnqueueParams) -> Result<Task> {
        text(&params.request_id, "request ID", 64, false)?;
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if let Some(queue) = &task.queue {
            if queue.request_id == params.request_id
                && queue.claude == params.claude
                && queue.codex == params.codex
                && queue.workspace == params.workspace
            {
                return Ok(task.clone());
            }
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "this task already has a background launch authorization".into(),
            });
        }
        if task.retired_launch_requests.contains(&params.request_id) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "this launch request was reset; use a new request ID".into(),
            });
        }
        if task.status != TaskStatus::Open
            || task.project.is_none()
            || task.launch.is_some()
            || !task.sessions.is_empty()
            || task.attention.is_some()
        {
            return Err(invalid(
                "enqueue requires an open project task without a session, launch or pending \
                 attention",
            ));
        }
        let mut updated = task.clone();
        updated.queue = Some(proto::TaskQueueRecord {
            task_id: params.task_id,
            request_id: params.request_id,
            claude: params.claude,
            codex: params.codex,
            workspace: params.workspace,
            state: proto::TaskQueueState::Queued,
            reason: None,
            session_id: None,
            enqueued_at_ms: now_ms(),
        });
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// The caller holds the task's launch lock, excluding a worker that could claim it.
    pub(crate) fn dequeue(&self, params: proto::TaskDequeueParams) -> Result<Task> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let queue =
            task.queue.as_ref().filter(|queue| queue.request_id == params.request_id).ok_or_else(
                || Error::Rpc {
                    code: proto::CONFLICT,
                    message: "request ID does not match the queued launch".into(),
                },
            )?;
        if queue.state == proto::TaskQueueState::Cancelled {
            return Ok(task.clone());
        }
        if task.launch.is_some() || queue.state == proto::TaskQueueState::Dispatched {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "the task was already claimed; inspect or stop its session".into(),
            });
        }
        let mut updated = task.clone();
        let queue = updated.queue.as_mut().ok_or(Error::Protocol("missing queue authorization"))?;
        if queue.state == proto::TaskQueueState::NeedsAttention
            && updated.attention.as_ref().is_some_and(|attention| {
                attention.review_id.is_none()
                    && attention.reason
                        == queue
                            .reason
                            .as_deref()
                            .unwrap_or("background launch requires reconciliation")
            })
        {
            updated.attention = None;
        }
        queue.state = proto::TaskQueueState::Cancelled;
        queue.reason = Some("cancelled before dispatch".into());
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// Persist each outcome before publishing it in memory. Terminal states never regress.
    pub(crate) fn queue_update(
        &self,
        id: &str,
        request_id: &str,
        state: proto::TaskQueueState,
        reason: Option<String>,
        session_id: Option<String>,
    ) -> Result<()> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let queue = task.queue.as_ref().filter(|queue| queue.request_id == request_id).ok_or_else(
            || Error::Rpc {
                code: proto::CONFLICT,
                message: "background launch authorization changed".into(),
            },
        )?;
        let reason = reason.map(|reason| reason.chars().take(1024).collect::<String>());
        if queue.state == state && queue.reason == reason && queue.session_id == session_id {
            return Ok(());
        }
        if !matches!(queue.state, proto::TaskQueueState::Queued | proto::TaskQueueState::Waiting)
            || !matches!(
                state,
                proto::TaskQueueState::Waiting
                    | proto::TaskQueueState::Dispatched
                    | proto::TaskQueueState::NeedsAttention
            )
            || (state == proto::TaskQueueState::Waiting && task.launch.is_some())
        {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "background launch outcome cannot be replayed or regressed".into(),
            });
        }
        if state == proto::TaskQueueState::Dispatched
            && (session_id.is_none()
                || task.launch.as_ref().map(|launch| launch.session_id.as_str())
                    != session_id.as_deref())
        {
            return Err(Error::Protocol("dispatched queue must name the claimed session"));
        }
        let mut updated = task.clone();
        let queue = updated.queue.as_mut().ok_or(Error::Protocol("missing queue authorization"))?;
        queue.state = state;
        queue.reason = reason.clone();
        queue.session_id = session_id;
        if state == proto::TaskQueueState::NeedsAttention && updated.attention.is_none() {
            updated.attention = Some(Attention {
                reason: reason
                    .unwrap_or_else(|| "background launch requires reconciliation".into()),
                review_id: None,
                at_ms: now_ms(),
            });
        }
        self.store.save_task(&updated)?;
        *task = updated;
        Ok(())
    }

    /// Authorize the service to drive the task's review-and-fix cycle. Repeating the same
    /// request returns the record; an active drive is never replaced, a finished one only by a
    /// new request ID.
    pub(crate) fn drive(&self, params: proto::TaskDriveParams) -> Result<Task> {
        text(&params.request_id, "request ID", 64, false)?;
        if !(1..=3600).contains(&params.timeout_secs) {
            return Err(invalid("timeout_secs must be 1..=3600"));
        }
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if let Some(drive) = &task.drive {
            if drive.params.request_id == params.request_id {
                return if drive.params == params {
                    Ok(task.clone())
                } else {
                    Err(Error::Rpc {
                        code: proto::CONFLICT,
                        message: "request ID reused with different drive parameters".into(),
                    })
                };
            }
            if drive.state.is_active() {
                return Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "the service is already driving this task".into(),
                });
            }
        }
        if task.status != TaskStatus::Open || task.attention.is_some() {
            return Err(invalid("drive requires an open task without pending attention"));
        }
        if let Some(session) = &params.session_id
            && !task.sessions.contains(session)
        {
            return Err(invalid("the selected session does not belong to the task"));
        }
        let now = now_ms();
        let mut updated = task.clone();
        updated.drive = Some(proto::TaskDriveRecord {
            params,
            state: proto::TaskDriveState::Driving,
            reason: None,
            session_id: None,
            progress: None,
            started_at_ms: now,
            updated_at_ms: now,
        });
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// Stop driving. Steps already taken stand; the cancellation is final for this request.
    pub(crate) fn undrive(&self, params: proto::TaskUndriveParams) -> Result<Task> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let drive = task
            .drive
            .as_ref()
            .filter(|drive| drive.params.request_id == params.request_id)
            .ok_or_else(|| Error::Rpc {
                code: proto::CONFLICT,
                message: "request ID does not match the task's drive".into(),
            })?;
        if !drive.state.is_active() {
            return Ok(task.clone());
        }
        let mut updated = task.clone();
        let drive = updated.drive.as_mut().ok_or(Error::Protocol("missing drive"))?;
        drive.state = proto::TaskDriveState::Cancelled;
        drive.reason = Some("cancelled".into());
        drive.updated_at_ms = now_ms();
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// Record a step of an active drive. Returns false, changing nothing, once the drive was
    /// cancelled, finished or replaced: their outcome wins over a worker's late step.
    pub(crate) fn drive_update(
        &self,
        id: &str,
        request_id: &str,
        update: impl FnOnce(&mut proto::TaskDriveRecord),
    ) -> Result<bool> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let Some(drive) = task
            .drive
            .as_ref()
            .filter(|drive| drive.params.request_id == request_id && drive.state.is_active())
        else {
            return Ok(false);
        };
        let mut changed = drive.clone();
        update(&mut changed);
        if let Some(reason) = &mut changed.reason {
            *reason = reason.chars().take(1024).collect();
        }
        changed.updated_at_ms = drive.updated_at_ms;
        if &changed == drive {
            return Ok(true);
        }
        changed.updated_at_ms = now_ms();
        let mut updated = task.clone();
        updated.drive = Some(changed);
        self.store.save_task(&updated)?;
        *task = updated;
        Ok(true)
    }

    /// Authorize landing the task's approved change. A repeat of the request returns the task.
    pub(crate) fn land(&self, params: proto::TaskLandParams) -> Result<Task> {
        text(&params.request_id, "request ID", 64, false)?;
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let workspace_base =
            task.queue.as_ref().and_then(|queue| queue.workspace.as_ref()).map(|w| &w.base_branch);
        let Some(target) = params.target_branch.as_ref().or(workspace_base).cloned() else {
            return Err(invalid("name the target branch: the task has no workspace base"));
        };
        crate::landings::target_ref(&target)
            .map_err(|_| invalid("the target must name a local branch (<=256 bytes)"))?;
        if let Some(landing) = &task.landing {
            if landing.request_id == params.request_id {
                return if landing.target_branch == target {
                    Ok(task.clone())
                } else {
                    Err(Error::Rpc {
                        code: proto::CONFLICT,
                        message: "request ID reused with a different target".into(),
                    })
                };
            }
            if landing.state != proto::TaskLandingState::Failed {
                return Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "the task is already landing or landed".into(),
                });
            }
        }
        if task.status != TaskStatus::Open {
            return Err(invalid("landing requires an open task"));
        }
        if task.drive.as_ref().is_some_and(|drive| drive.state.is_active()) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "changes are under way: the service is still driving this task".into(),
            });
        }
        let now = now_ms();
        let mut updated = task.clone();
        updated.landing = Some(proto::TaskLandingRecord {
            request_id: params.request_id,
            target_branch: target,
            state: proto::TaskLandingState::Landing,
            reason: None,
            preview_id: None,
            started_at_ms: now,
            updated_at_ms: now,
            workspace: None,
        });
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// Ask for changes to the change `review` (the task's latest) judged: record the request and
    /// restart the task's drive at a fix turn that sends it. A repeat of the request returns the
    /// task.
    pub(crate) fn revise(
        &self,
        params: proto::TaskReviseParams,
        review: Option<proto::Review>,
    ) -> Result<Task> {
        text(&params.request_id, "request ID", 64, false)?;
        let feedback = params.feedback.trim();
        text(feedback, "the requested changes", proto::TASK_MAX_FEEDBACK_BYTES, true)?;
        if feedback.len() > proto::TASK_MAX_FEEDBACK_BYTES {
            return Err(invalid("the requested changes must fit in 6000 bytes"));
        }
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if let Some(earlier) = task.revisions.iter().find(|r| r.request_id == params.request_id) {
            return if earlier.feedback == feedback {
                Ok(task.clone())
            } else {
                Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "request ID reused with other requested changes".into(),
                })
            };
        }
        if task.status != TaskStatus::Open || task.attention.is_some() {
            return Err(invalid("changes are asked of an open task without pending attention"));
        }
        if task.landing.as_ref().is_some_and(|landing| {
            matches!(
                landing.state,
                proto::TaskLandingState::Landing | proto::TaskLandingState::Landed
            )
        }) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "the task is landing or landed".into(),
            });
        }
        let Some(drive) = &task.drive else {
            return Err(invalid(
                "only a task the service drives takes requested changes; send them to its session",
            ));
        };
        if drive.state.is_active() {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "changes are under way: the service is still driving this task".into(),
            });
        }
        let session = drive
            .session_id
            .as_ref()
            .ok_or_else(|| invalid("the drive has no implementing session yet"))?;
        let review = review
            .filter(|review| {
                review.status == proto::ReviewStatus::Completed
                    && review.task_id.as_deref() == Some(task.id.as_str())
                    && &review.session_id == session
            })
            .ok_or_else(|| invalid("the task has no completed review of its session to revise"))?;
        let now = now_ms();
        let digest = sha256(&format!("{}\n{}", task.id, params.request_id));
        let progress = crate::drive::revise(
            drive.progress.as_ref(),
            &review.id,
            format!("task-revise-{}", &digest[..32]),
            &params.request_id,
            now,
        )?;
        let mut updated = task.clone();
        updated.revisions.push(proto::TaskRevision {
            request_id: params.request_id,
            feedback: feedback.to_owned(),
            review_id: review.id,
            checkpoint_id: review.checkpoint_id,
            requested_at_ms: now,
        });
        let drive = updated.drive.as_mut().ok_or(Error::Protocol("missing drive"))?;
        drive.state = proto::TaskDriveState::Driving;
        drive.reason = None;
        drive.progress = Some(progress);
        drive.updated_at_ms = now;
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    /// Record a step of an active landing; `false` once it is no longer this request's.
    pub(crate) fn landing_update(
        &self,
        id: &str,
        request_id: &str,
        update: impl FnOnce(&mut proto::TaskLandingRecord),
    ) -> Result<bool> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let Some(landing) = task.landing.as_ref().filter(|landing| {
            landing.request_id == request_id && landing.state == proto::TaskLandingState::Landing
        }) else {
            return Ok(false);
        };
        let mut changed = landing.clone();
        update(&mut changed);
        if let Some(reason) = &mut changed.reason {
            *reason = reason.chars().take(1024).collect();
        }
        changed.updated_at_ms = landing.updated_at_ms;
        if &changed == landing {
            return Ok(true);
        }
        changed.updated_at_ms = now_ms();
        let mut updated = task.clone();
        updated.landing = Some(changed);
        self.store.save_task(&updated)?;
        *task = updated;
        Ok(true)
    }

    /// Record what became of a landed task's workspace; `false` once the landing is not this
    /// request's, or the outcome is already final.
    pub(crate) fn workspace_update(
        &self,
        id: &str,
        request_id: &str,
        cleanup: proto::TaskWorkspaceCleanup,
    ) -> Result<bool> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let Some(landing) = task.landing.as_ref().filter(|landing| {
            landing.request_id == request_id
                && landing.state == proto::TaskLandingState::Landed
                && landing_pending(landing)
        }) else {
            return Ok(false);
        };
        let mut changed = landing.clone();
        let mut cleanup = cleanup;
        if let Some(reason) = &mut cleanup.reason {
            *reason = reason.chars().take(1024).collect();
        }
        changed.workspace = Some(cleanup);
        changed.updated_at_ms = now_ms();
        let mut updated = task.clone();
        updated.landing = Some(changed);
        self.store.save_task(&updated)?;
        *task = updated;
        Ok(true)
    }

    pub(crate) fn pending_landings(&self) -> Vec<(String, proto::TaskLandingRecord)> {
        let mut pending: Vec<_> = lock(&self.tasks)
            .values()
            .filter_map(|task| Some((task.id.clone(), task.landing.clone()?)))
            .filter(|(_, landing)| landing_pending(landing))
            .collect();
        pending.sort_by(|a, b| a.1.started_at_ms.cmp(&b.1.started_at_ms).then(a.0.cmp(&b.0)));
        pending
    }

    pub(crate) fn pending_drives(&self) -> Vec<proto::TaskDriveRecord> {
        let mut pending: Vec<_> = lock(&self.tasks)
            .values()
            .filter_map(|task| task.drive.as_ref())
            .filter(|drive| drive.state.is_active())
            .cloned()
            .collect();
        pending.sort_by(|a, b| {
            a.started_at_ms.cmp(&b.started_at_ms).then(a.params.task_id.cmp(&b.params.task_id))
        });
        pending
    }

    /// A drive or a landing still has steps to take.
    pub(crate) fn has_pending_drive(&self) -> bool {
        lock(&self.tasks).values().any(|task| {
            task.drive.as_ref().is_some_and(|drive| drive.state.is_active())
                || task.landing.as_ref().is_some_and(landing_pending)
        })
    }

    pub(crate) fn pending_queue(&self) -> Vec<proto::TaskQueueRecord> {
        let mut pending: Vec<_> = lock(&self.tasks)
            .values()
            .filter(|task| task.status == TaskStatus::Open)
            .filter_map(|task| task.queue.as_ref())
            .filter(|queue| {
                matches!(
                    queue.state,
                    proto::TaskQueueState::Queued | proto::TaskQueueState::Waiting
                )
            })
            .cloned()
            .collect();
        pending.sort_by(|a, b| {
            a.enqueued_at_ms.cmp(&b.enqueued_at_ms).then(a.task_id.cmp(&b.task_id))
        });
        pending
    }

    pub(crate) fn has_pending_queue(&self) -> bool {
        lock(&self.tasks).values().any(|task| {
            task.status == TaskStatus::Open
                && task.queue.as_ref().is_some_and(|queue| {
                    matches!(
                        queue.state,
                        proto::TaskQueueState::Queued | proto::TaskQueueState::Waiting
                    )
                })
        })
    }

    pub fn create(&self, params: TaskCreateParams) -> Result<Task> {
        let task = self.draft(params)?;
        let mut tasks = lock(&self.tasks);
        if let Some(owner) = task.sessions.iter().find_map(|session| open_task(&tasks, session)) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: format!("a session already belongs to open task {}", owner.id),
            });
        }
        // Edges only point to existing tasks and are immutable, so a cycle cannot be created.
        for prerequisite in &task.depends_on {
            if !tasks.contains_key(prerequisite) {
                return Err(invalid("unknown prerequisite task"));
            }
        }
        self.store.save_task(&task)?;
        tasks.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    /// The task a delegation request created: `None` if there is none yet, a conflict if the
    /// request ID was used with other content.
    pub(crate) fn delegated(&self, request_id: &str, input_hash: &str) -> Result<Option<Task>> {
        text(request_id, "request ID", 64, false)?;
        let tasks = lock(&self.tasks);
        let Some(task) = tasks.values().find(|task| {
            task.delegation.as_ref().is_some_and(|delegation| delegation.request_id == request_id)
        }) else {
            return Ok(None);
        };
        if task.delegation.as_ref().is_some_and(|delegation| delegation.input_hash != input_hash) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "request ID reused with a different delegation".into(),
            });
        }
        Ok(Some(task.clone()))
    }

    /// Store a drafted task that carries its delegation, queue and drive, all in one record, so
    /// no crash can leave part of a delegation behind.
    pub(crate) fn insert_delegated(&self, task: Task) -> Result<Task> {
        let Some(delegation) = task.delegation.clone() else {
            return Err(Error::Protocol("a delegated task records its request"));
        };
        let mut tasks = lock(&self.tasks);
        let earlier = tasks.values().find(|task| {
            task.delegation.as_ref().is_some_and(|other| other.request_id == delegation.request_id)
        });
        if let Some(earlier) = earlier {
            // A concurrent identical request got here first.
            return if earlier.delegation == Some(delegation) {
                Ok(earlier.clone())
            } else {
                Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "request ID reused with a different delegation".into(),
                })
            };
        }
        self.store.save_task(&task)?;
        tasks.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    /// A validated task with a new ID, not yet stored; sessions and prerequisites are checked
    /// against the table when it is.
    pub(crate) fn draft(&self, params: TaskCreateParams) -> Result<Task> {
        text(&params.title, "title", proto::TASK_MAX_TITLE, false)?;
        text(&params.goal, "goal", proto::TASK_MAX_GOAL, true)?;
        if let Some(constraints) = &params.constraints {
            text(constraints, "constraints", proto::TASK_MAX_CONSTRAINTS, true)?;
        }
        if params.criteria.is_empty() || params.criteria.len() > proto::TASK_MAX_CRITERIA {
            return Err(invalid("a task needs 1-20 acceptance criteria"));
        }
        for criterion in &params.criteria {
            text(criterion, "each criterion", proto::TASK_MAX_CRITERION, false)?;
        }
        let fix_budget = params.fix_budget.unwrap_or(proto::TASK_DEFAULT_FIX_BUDGET);
        if fix_budget > proto::TASK_MAX_FIX_BUDGET {
            return Err(invalid("the fix budget is 0-5 rounds"));
        }
        let mut sessions = params.sessions.clone();
        sessions.dedup();
        let stopping = std::sync::atomic::AtomicBool::new(false);
        let project =
            params.project.as_deref().map(|cwd| repository(cwd, &stopping)).transpose()?;
        for id in &sessions {
            let session = self.sessions.get(id)?;
            if let Some(project) = &project
                && repository(&session.cwd, &stopping)? != *project
            {
                return Err(invalid("task sessions must belong to the selected project"));
            }
        }
        let mut depends_on = params.depends_on;
        depends_on.sort();
        depends_on.dedup();
        Ok(Task {
            revisions: Vec::new(),
            landing: None,
            delegation: None,
            drive: None,
            queue: None,
            launch: None,
            retired_launch_requests: Vec::new(),
            project,
            depends_on,
            id: format!("task-{}", epoch()?),
            title: params.title,
            goal: params.goal,
            constraints: params.constraints,
            criteria: params
                .criteria
                .iter()
                .enumerate()
                .map(|(index, text)| Criterion {
                    id: format!("AC{}", index + 1),
                    text: text.clone(),
                    hash: sha256(text),
                })
                .collect(),
            sessions,
            status: TaskStatus::Open,
            ledger: Vec::new(),
            fix_budget,
            dual_review: params.dual_review,
            fix_rounds: Vec::new(),
            attention: None,
            invalid_streak: 0,
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        })
    }

    pub fn claim_launch(
        &self,
        id: &str,
        request_id: &str,
        session_id: &str,
        input_hash: String,
    ) -> Result<(proto::TaskLaunch, bool)> {
        text(request_id, "request ID", 64, false)?;
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if task.retired_launch_requests.iter().any(|retired| retired == request_id) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "this launch request was reset; use a new request ID".into(),
            });
        }
        if let Some(launch) = &task.launch {
            if launch.request_id == request_id && launch.input_hash == input_hash {
                return Ok((launch.clone(), false));
            }
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: format!("task already claimed for session {}", launch.session_id),
            });
        }
        if let Some(queue) = &task.queue {
            let authorised = match queue.state {
                proto::TaskQueueState::Queued | proto::TaskQueueState::Waiting => {
                    let queued_hash =
                        launch_hash(&queued_engine(queue)?, queue.workspace.as_ref())?;
                    queue.request_id == request_id && queued_hash == input_hash
                },
                proto::TaskQueueState::Cancelled => queue.request_id != request_id,
                proto::TaskQueueState::NeedsAttention | proto::TaskQueueState::Dispatched => false,
            };
            if !authorised {
                return Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "task has a background launch authorization; inspect its queue record"
                        .into(),
                });
            }
        }
        if task.status != TaskStatus::Open || !task.sessions.is_empty() || task.attention.is_some()
        {
            return Err(invalid(
                "launch requires an open task without a session or pending attention",
            ));
        }
        let launch = proto::TaskLaunch {
            may_have_started: false,
            request_id: request_id.into(),
            session_id: session_id.into(),
            input_hash,
        };
        let mut updated = task.clone();
        updated.launch = Some(launch.clone());
        updated.sessions.push(session_id.into());
        self.store.save_task(&updated)?;
        *task = updated;
        Ok((launch, true))
    }

    /// The caller holds the launch lock across this durable transition and the engine start.
    pub fn mark_launch_started(&self, id: &str, launch: &proto::TaskLaunch) -> Result<()> {
        let mut tasks = lock(&self.tasks);
        let task = tasks.get_mut(id).ok_or_else(|| invalid("unknown task"))?;
        if task.status != TaskStatus::Open || task.launch.as_ref() != Some(launch) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "task changed before starting its session".into(),
            });
        }
        let mut updated = task.clone();
        updated.launch = Some(proto::TaskLaunch { may_have_started: true, ..launch.clone() });
        self.store.save_task(&updated)?;
        *task = updated;
        Ok(())
    }

    pub fn reset_launch(
        &self,
        runtime: &crate::runtime::Runtime,
        params: proto::TaskResetLaunchParams,
    ) -> Result<Task> {
        let _guard = runtime.lock_task_launch(&params.task_id)?;
        let snapshot = self.get(&params.task_id)?;
        if snapshot.retired_launch_requests.contains(&params.request_id) {
            return Ok(snapshot);
        }
        let conflict =
            |message: &str| Error::Rpc { code: proto::CONFLICT, message: message.into() };
        let launch = snapshot
            .launch
            .as_ref()
            .filter(|launch| launch.request_id == params.request_id)
            .ok_or_else(|| conflict("request ID does not match the current launch"))?;
        match self.sessions.get(&launch.session_id) {
            Err(Error::Rpc { code: proto::NOT_FOUND, .. }) => (),
            Ok(_) => return Err(conflict("a session record exists; inspect that session")),
            Err(error) => return Err(error),
        }
        if launch.may_have_started {
            return Err(conflict("the daemon cannot prove that the previous launch never started"));
        }
        if self.store.has_session(&launch.session_id)? {
            return Err(conflict("a durable session record exists; inspect that session"));
        }
        // The launcher records session ownership before spawning. An empty directory can be
        // left by a crash before that write. Any leftover contents require separate inspection.
        let path = runtime.directory().join(&launch.session_id);
        match std::fs::read_dir(&path) {
            Ok(mut entries) => {
                if entries.next().transpose()?.is_some() {
                    return Err(conflict(
                        "launch directory contains evidence; inspect that session",
                    ));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get_mut(&params.task_id)
            .ok_or_else(|| conflict("task disappeared while reconciling"))?;
        if task.status != TaskStatus::Open || task.launch.as_ref() != Some(launch) {
            return Err(conflict("task changed while reconciling"));
        }
        let mut updated = task.clone();
        updated.launch = None;
        updated.sessions.retain(|id| id != &launch.session_id);
        updated.retired_launch_requests.push(params.request_id);
        self.store.save_task(&updated)?;
        *task = updated.clone();
        Ok(updated)
    }

    pub fn get(&self, id: &str) -> Result<Task> {
        lock(&self.tasks)
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = lock(&self.tasks).values().cloned().collect();
        tasks.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(a.id.cmp(&b.id)));
        tasks
    }

    pub fn close(&self, id: &str, status: TaskStatus) -> Result<Task> {
        if status == TaskStatus::Open {
            return Err(invalid("a task closes as accepted or abandoned"));
        }
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if task.status != TaskStatus::Open {
            return Err(Error::Rpc { code: proto::CONFLICT, message: "the task is closed".into() });
        }
        let mut closed = Task { status, ..task };
        if closed.launch.is_none()
            && let Some(queue) = &mut closed.queue
            && matches!(queue.state, proto::TaskQueueState::Queued | proto::TaskQueueState::Waiting)
        {
            queue.state = proto::TaskQueueState::Cancelled;
            queue.reason = Some("task closed before dispatch".into());
        }
        self.store.save_task(&closed)?;
        tasks.insert(closed.id.clone(), closed.clone());
        Ok(closed)
    }

    pub fn for_session(&self, session: &str) -> Option<Task> {
        open_task(&lock(&self.tasks), session).cloned()
    }

    /// The task a review of `session`'s checkpoint may use: open and including that session.
    pub fn for_review(&self, id: &str, session: &str) -> Result<Task> {
        let task = self.get(id)?;
        if task.status != TaskStatus::Open {
            return Err(Error::Rpc { code: proto::CONFLICT, message: "the task is closed".into() });
        }
        if !task.sessions.iter().any(|s| s == session) {
            return Err(invalid("the checkpoint's session is not part of this task"));
        }
        Ok(task)
    }
}

impl Tasks {
    /// Record a completed review of its task: the ledger (unless the review is invalid), the
    /// invalid streak, and the circuit breakers that stop the loop for a person.
    pub fn record_review(&self, review: &Review) -> Result<()> {
        let Some(id) = &review.task_id else { return Ok(()) };
        let mut tasks = lock(&self.tasks);
        let Some(task) = tasks.get(id) else { return Ok(()) };
        let mut updated = task.clone();
        let trip = |updated: &mut Task, reason: String| {
            if updated.attention.is_none() {
                updated.attention =
                    Some(Attention { reason, review_id: Some(review.id.clone()), at_ms: now_ms() });
            }
        };
        if review.verdict == Some(ReviewVerdict::Invalid) {
            updated.invalid_streak += 1;
            if updated.invalid_streak >= 2 {
                trip(&mut updated, "the reviewer gave invalid output twice in a row".into());
            }
        } else {
            updated.invalid_streak = 0;
            updated.ledger = ledger(&task.ledger, &review.id, &review.findings);
            if let Some(round) = task.fix_rounds.last()
                && round.review_id != review.id
                && review.created_at_ms >= round.at_ms
            {
                if review.tree == round.tree {
                    trip(
                        &mut updated,
                        format!(
                            "the fix round changed nothing: review {} found the same tree as \
                             round {}",
                            review.id,
                            task.fix_rounds.len()
                        ),
                    );
                } else if let Some(survivor) = review.findings.iter().find(|finding| {
                    let key = ledger_key(&finding.path, &finding.title);
                    finding.blocking
                        && round.blocking.contains(&key)
                        && !is_disputed(&updated.ledger, &key)
                }) {
                    trip(
                        &mut updated,
                        format!("blocking finding \"{}\" survived a fix round", survivor.title),
                    );
                }
            }
        }
        self.store.save_task(&updated)?;
        tasks.insert(updated.id.clone(), updated);
        Ok(())
    }

    /// Ledger keys of the task's disputed findings.
    pub fn disputed_keys(&self, id: &str) -> BTreeSet<String> {
        lock(&self.tasks).get(id).map_or_else(BTreeSet::new, |task| {
            task.ledger
                .iter()
                .filter(|entry| entry.status == LedgerStatus::Disputed)
                .map(|entry| entry.key.clone())
                .collect()
        })
    }

    /// Claim a fix round before a review is sent back: refused while the task needs attention,
    /// when nothing is left to send but disputed findings, or when the budget is used up (the
    /// last two also set the need for attention).
    pub fn begin_fix(&self, id: &str, round: FixRound, has_content: bool) -> Result<()> {
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if task.status != TaskStatus::Open {
            return Err(Error::Rpc { code: proto::CONFLICT, message: "the task is closed".into() });
        }
        if task.fix_rounds.iter().any(|fix| fix.review_id == round.review_id) {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "this review already has a fix round".into(),
            });
        }
        if let Some(attention) = &task.attention {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: format!(
                    "the task needs attention: {}; run xaya task continue {} after deciding",
                    attention.reason, task.id
                ),
            });
        }
        let refusal = if !has_content {
            Some("only disputed findings remain; a person must decide".to_owned())
        } else if task.fix_rounds.len() as u32 >= task.fix_budget {
            Some(format!("the fix budget of {} round(s) is used up", task.fix_budget))
        } else {
            None
        };
        let mut updated = task;
        if let Some(reason) = refusal {
            updated.attention = Some(Attention {
                reason: reason.clone(),
                review_id: Some(round.review_id),
                at_ms: now_ms(),
            });
            self.store.save_task(&updated)?;
            tasks.insert(updated.id.clone(), updated);
            return Err(Error::Rpc { code: proto::CONFLICT, message: reason });
        }
        updated.fix_rounds.push(round);
        self.store.save_task(&updated)?;
        tasks.insert(updated.id.clone(), updated);
        Ok(())
    }

    /// Release a claimed round whose send was refused before anything was dispatched.
    pub fn cancel_fix(&self, id: &str, review: &str) -> Result<()> {
        let mut tasks = lock(&self.tasks);
        let Some(task) = tasks.get(id) else { return Ok(()) };
        if task.fix_rounds.last().is_none_or(|round| round.review_id != review) {
            return Ok(());
        }
        let mut updated = task.clone();
        updated.fix_rounds.pop();
        self.store.save_task(&updated)?;
        tasks.insert(updated.id.clone(), updated);
        Ok(())
    }

    /// Mark a finding of `review` disputed. The verdict of every review stays as decided.
    pub fn dispute(
        &self,
        id: &str,
        review: &str,
        finding: &ReviewFinding,
        reason: &str,
    ) -> Result<Task> {
        text(reason, "a dispute reason", 500, true)?;
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        let key = ledger_key(&finding.path, &finding.title);
        let dispute = Dispute { reason: reason.into(), review_id: review.into(), at_ms: now_ms() };
        let mut updated = task;
        match updated.ledger.iter_mut().find(|entry| entry.key == key) {
            Some(entry) => {
                entry.status = LedgerStatus::Disputed;
                entry.dispute = Some(dispute);
            },
            None => updated.ledger.push(LedgerEntry {
                key,
                path: finding.path.clone(),
                title: finding.title.clone(),
                status: LedgerStatus::Disputed,
                first_review: review.into(),
                last_review: review.into(),
                seen: 1,
                blocking: finding.blocking,
                dispute: Some(dispute),
            }),
        }
        self.store.save_task(&updated)?;
        tasks.insert(updated.id.clone(), updated.clone());
        Ok(updated)
    }

    /// A person has decided: clear the need for attention and grant `rounds` more fixes.
    pub fn continue_(&self, id: &str, rounds: u32) -> Result<Task> {
        if rounds > proto::TASK_MAX_FIX_BUDGET {
            return Err(invalid("at most 5 more rounds at a time"));
        }
        let mut tasks = lock(&self.tasks);
        let task = tasks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Rpc { code: proto::NOT_FOUND, message: "unknown task".into() })?;
        if task.status != TaskStatus::Open {
            return Err(Error::Rpc { code: proto::CONFLICT, message: "the task is closed".into() });
        }
        if task.attention.is_none() && rounds == 0 {
            return Err(Error::Rpc {
                code: proto::CONFLICT,
                message: "the task does not need attention; pass rounds to extend its budget"
                    .into(),
            });
        }
        let updated = Task {
            attention: None,
            fix_budget: task.fix_budget.saturating_add(rounds),
            invalid_streak: 0,
            ..task
        };
        self.store.save_task(&updated)?;
        tasks.insert(updated.id.clone(), updated.clone());
        Ok(updated)
    }
}

fn is_disputed(ledger: &[LedgerEntry], key: &str) -> bool {
    ledger.iter().any(|entry| entry.key == key && entry.status == LedgerStatus::Disputed)
}

/// Binds a claim to its exact launch. A launch in the project's own checkout keeps the hash
/// earlier claims were stored with.
/// The criterion of a delegated task: the request itself is what the review judges.
pub(crate) const DELEGATED_CRITERION: &str = "The change does what the goal asks, completely.";

/// A delegated task's title: the goal's first non-blank line, shortened to fit.
pub(crate) fn title_of(goal: &str) -> String {
    let line = goal.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    let line: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if line.chars().count() <= proto::TASK_MAX_TITLE {
        return line;
    }
    let mut title: String = line.chars().take(proto::TASK_MAX_TITLE - 1).collect();
    title.truncate(title.trim_end().len());
    title.push('…');
    title
}

/// The first message an implementing engine receives for a task.
pub fn prompt(task: &Task) -> String {
    let criteria =
        task.criteria.iter().map(|item| format!("- {}", item.text)).collect::<Vec<_>>().join("\n");
    let constraints = task
        .constraints
        .as_deref()
        .map(|constraints| format!("Constraints: {constraints}\n\n"))
        .unwrap_or_default();
    format!("{}\n\n{}\n\n{constraints}Acceptance criteria:\n{criteria}", task.title, task.goal)
}

pub(crate) fn launch_hash(
    engine: &crate::storage::EngineParams,
    workspace: Option<&proto::TaskWorkspace>,
) -> Result<String> {
    Ok(sha256(&match workspace {
        None => serde_json::to_string(engine)?,
        Some(workspace) => {
            serde_json::to_string(&serde_json::json!({"engine":engine,"workspace":workspace}))?
        },
    }))
}

/// The launch a queue entry authorizes.
pub(crate) fn queued_engine(
    queue: &proto::TaskQueueRecord,
) -> Result<crate::storage::EngineParams> {
    match (&queue.claude, &queue.codex) {
        (Some(claude), None) => Ok(crate::storage::EngineParams::Claude(claude.clone())),
        (None, Some(codex)) => Ok(crate::storage::EngineParams::Codex(codex.clone())),
        _ => Err(Error::Protocol("a queued launch names exactly one engine")),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// A finding's identity across reviews: its path and its title without the priority tag, case
/// or extra spacing. Line numbers are left out because fixes move them.
pub(crate) fn ledger_key(path: &str, title: &str) -> String {
    let mut title = title.trim();
    if let Some(rest) = title.strip_prefix("[P")
        && rest.len() > 1
        && rest.as_bytes()[0].is_ascii_digit()
        && rest.as_bytes()[1] == b']'
    {
        title = &rest[2..];
    }
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    format!("{path}::{title}")
}

/// `previous` after a completed review that reported `findings`.
fn ledger(previous: &[LedgerEntry], review: &str, findings: &[ReviewFinding]) -> Vec<LedgerEntry> {
    let mut entries: Vec<LedgerEntry> = previous.to_vec();
    let mut reported = std::collections::BTreeSet::new();
    for finding in findings {
        let key = ledger_key(&finding.path, &finding.title);
        if !reported.insert(key.clone()) {
            continue;
        }
        match entries.iter_mut().find(|entry| entry.key == key) {
            Some(entry) => {
                if entry.status == LedgerStatus::Resolved {
                    entry.status = LedgerStatus::Reappeared;
                }
                // A disputed finding stays disputed until a person decides.
                entry.title = finding.title.clone();
                entry.last_review = review.into();
                entry.seen += 1;
                entry.blocking = finding.blocking;
            },
            None => entries.push(LedgerEntry {
                key,
                path: finding.path.clone(),
                title: finding.title.clone(),
                status: LedgerStatus::Open,
                first_review: review.into(),
                last_review: review.into(),
                seen: 1,
                blocking: finding.blocking,
                dispute: None,
            }),
        }
    }
    for entry in &mut entries {
        if !reported.contains(&entry.key)
            && !matches!(entry.status, LedgerStatus::Resolved | LedgerStatus::Disputed)
        {
            entry.status = LedgerStatus::Resolved;
        }
    }
    entries
}

fn open_task<'a>(tasks: &'a BTreeMap<String, Task>, session: &str) -> Option<&'a Task> {
    tasks
        .values()
        .find(|task| task.status == TaskStatus::Open && task.sessions.iter().any(|s| s == session))
}

/// A landing still has steps to take: landing the change, or then cleaning up its workspace.
fn landing_pending(landing: &proto::TaskLandingRecord) -> bool {
    match landing.state {
        proto::TaskLandingState::Landing => true,
        proto::TaskLandingState::Landed => landing
            .workspace
            .as_ref()
            .is_none_or(|cleanup| cleanup.outcome == proto::TaskWorkspaceOutcome::Removing),
        proto::TaskLandingState::Failed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegated_title_is_the_goals_first_line_and_fits() {
        assert_eq!(title_of("\n  Fix the login page  \nmore detail"), "Fix the login page");
        assert_eq!(title_of("修复登录\t页面"), "修复登录 页面");
        let long = "字".repeat(200);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), proto::TASK_MAX_TITLE);
        assert!(title.ends_with('…'));
        assert!(text(&title, "title", proto::TASK_MAX_TITLE, false).is_ok());
    }

    fn finding(path: &str, title: &str, blocking: bool) -> ReviewFinding {
        ReviewFinding {
            title: title.into(),
            body: String::new(),
            priority: None,
            confidence: 0.9,
            path: path.into(),
            start: 1,
            end: 1,
            blocking,
        }
    }

    fn queue_fixture() -> (tempfile::TempDir, Arc<Store>, Tasks, proto::TaskEnqueueParams) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        let sessions = Arc::new(Sessions::default());
        let tasks = Tasks::open(Arc::clone(&store), sessions).unwrap();
        let task: Task = serde_json::from_value(serde_json::json!({
            "id":"task-queue", "title":"t", "goal":"g", "constraints":null,
            "criteria":[], "sessions":[], "status":"open", "created_at_ms":1,
            "project":"/repo/.git"
        }))
        .unwrap();
        store.save_task(&task).unwrap();
        lock(&tasks.tasks).insert(task.id.clone(), task);
        let params = proto::TaskEnqueueParams {
            task_id: "task-queue".into(),
            request_id: "queue-request".into(),
            claude: Some(proto::StartClaudeParams {
                cwd: "/repo".into(),
                executable: "/bin/true".into(),
                config_dir: None,
                prompt: "implement".into(),
            }),
            codex: None,
            workspace: None,
        };
        (temp, store, tasks, params)
    }

    #[test]
    fn queue_update_failure_keeps_memory_and_storage_and_identical_wait_is_a_noop() {
        let (temp, store, tasks, params) = queue_fixture();
        tasks.enqueue(params.clone()).unwrap();
        tasks
            .queue_update(
                &params.task_id,
                &params.request_id,
                proto::TaskQueueState::Waiting,
                Some("capacity full".into()),
                None,
            )
            .unwrap();
        let before = tasks.get(&params.task_id).unwrap();
        let connection =
            rusqlite::Connection::open(temp.path().join("run/state/agentd.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_task AFTER UPDATE ON tasks BEGIN SELECT RAISE(FAIL, 'write \
                 refused'); END;",
            )
            .unwrap();
        tasks
            .queue_update(
                &params.task_id,
                &params.request_id,
                proto::TaskQueueState::Waiting,
                Some("capacity full".into()),
                None,
            )
            .unwrap();
        assert!(
            tasks
                .queue_update(
                    &params.task_id,
                    &params.request_id,
                    proto::TaskQueueState::NeedsAttention,
                    Some("engine failed".into()),
                    None
                )
                .is_err()
        );
        assert_eq!(tasks.get(&params.task_id).unwrap(), before);
        let restored = Tasks::open(store, Arc::new(Sessions::default())).unwrap();
        assert_eq!(restored.get(&params.task_id).unwrap(), before);
    }

    #[test]
    fn queue_write_failure_does_not_authorize_dispatch_after_restart() {
        let (temp, store, tasks, params) = queue_fixture();
        let connection =
            rusqlite::Connection::open(temp.path().join("run/state/agentd.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_task AFTER UPDATE ON tasks BEGIN SELECT RAISE(FAIL, 'write \
                 refused'); END;",
            )
            .unwrap();
        assert!(tasks.enqueue(params).is_err());
        assert!(!tasks.has_pending_queue());
        let restored = Tasks::open(store, Arc::new(Sessions::default())).unwrap();
        assert!(
            !restored.has_pending_queue(),
            "an operation reported as failed must not authorize a later launch"
        );
    }

    #[test]
    fn queue_attention_can_be_cancelled_without_reviving_its_authorization() {
        let (_temp, _store, tasks, params) = queue_fixture();
        tasks.enqueue(params.clone()).unwrap();
        tasks
            .queue_update(
                &params.task_id,
                &params.request_id,
                proto::TaskQueueState::NeedsAttention,
                Some("missing executable".into()),
                None,
            )
            .unwrap();
        let task = tasks.get(&params.task_id).unwrap();
        assert_eq!(task.attention.unwrap().reason, "missing executable");
        assert!(!tasks.has_pending_queue());
        let cancelled = tasks
            .dequeue(proto::TaskDequeueParams {
                task_id: params.task_id.clone(),
                request_id: params.request_id.clone(),
            })
            .unwrap();
        assert!(cancelled.attention.is_none());
        assert_eq!(tasks.enqueue(params.clone()).unwrap(), cancelled);
        assert!(
            tasks.claim_launch(&params.task_id, &params.request_id, "late", "hash".into()).is_err()
        );
        tasks.claim_launch(&params.task_id, "manual", "s", "hash".into()).unwrap();
    }

    #[test]
    fn queue_close_cancels_only_intent_that_was_not_claimed() {
        let (_temp, _store, tasks, params) = queue_fixture();
        tasks.enqueue(params.clone()).unwrap();
        let task = tasks.close(&params.task_id, TaskStatus::Abandoned).unwrap();
        assert_eq!(task.queue.unwrap().state, proto::TaskQueueState::Cancelled);
        assert!(!tasks.has_pending_queue());
    }

    #[test]
    fn queue_intent_is_idempotent_immutable_and_survives_restart() {
        let (_temp, store, tasks, params) = queue_fixture();
        let queued = tasks.enqueue(params.clone()).unwrap();
        assert_eq!(queued.queue.as_ref().unwrap().state, proto::TaskQueueState::Queued);
        assert_eq!(tasks.enqueue(params.clone()).unwrap(), queued);
        let mut changed = params.clone();
        changed.claude.as_mut().unwrap().prompt = "other".into();
        assert!(tasks.enqueue(changed).is_err());
        let restored = Tasks::open(store, Arc::new(Sessions::default())).unwrap();
        assert_eq!(restored.get(&params.task_id).unwrap(), queued);
        assert_eq!(restored.pending_queue().len(), 1);
        assert!(restored.has_pending_queue());
    }

    #[test]
    fn queue_cancellation_cannot_be_revived_or_dispatched_by_a_late_request() {
        let (_temp, store, tasks, params) = queue_fixture();
        tasks.enqueue(params.clone()).unwrap();
        let cancel = proto::TaskDequeueParams {
            task_id: params.task_id.clone(),
            request_id: params.request_id.clone(),
        };
        let cancelled = tasks.dequeue(cancel.clone()).unwrap();
        assert_eq!(cancelled.queue.as_ref().unwrap().state, proto::TaskQueueState::Cancelled);
        assert_eq!(tasks.dequeue(cancel).unwrap(), cancelled);
        assert_eq!(tasks.enqueue(params.clone()).unwrap(), cancelled);
        assert!(!tasks.has_pending_queue());
        assert!(
            tasks.claim_launch(&params.task_id, &params.request_id, "late", "hash".into()).is_err()
        );
        let restored = Tasks::open(store, Arc::new(Sessions::default())).unwrap();
        assert!(!restored.has_pending_queue());
        assert!(
            restored
                .claim_launch(&params.task_id, "manual-request", "manual", "manual-hash".into())
                .is_ok()
        );
        assert_eq!(
            restored
                .dequeue(proto::TaskDequeueParams {
                    task_id: params.task_id,
                    request_id: params.request_id
                })
                .unwrap()
                .queue
                .unwrap()
                .state,
            proto::TaskQueueState::Cancelled
        );
    }

    #[test]
    fn queue_launch_reserves_its_exact_request_and_payload_and_does_not_regress() {
        let (_temp, _store, tasks, params) = queue_fixture();
        tasks.enqueue(params.clone()).unwrap();
        assert!(
            tasks
                .queue_update(
                    &params.task_id,
                    &params.request_id,
                    proto::TaskQueueState::Dispatched,
                    None,
                    None
                )
                .is_err()
        );
        let hash = sha256(
            &serde_json::to_string(&crate::storage::EngineParams::Claude(
                params.claude.clone().unwrap(),
            ))
            .unwrap(),
        );
        assert!(tasks.claim_launch(&params.task_id, "other", "s", hash.clone()).is_err());
        assert!(
            tasks.claim_launch(&params.task_id, &params.request_id, "s", "wrong".into()).is_err()
        );
        let (launch, fresh) =
            tasks.claim_launch(&params.task_id, &params.request_id, "s", hash.clone()).unwrap();
        assert!(fresh);
        tasks
            .queue_update(
                &params.task_id,
                &params.request_id,
                proto::TaskQueueState::Dispatched,
                None,
                Some("s".into()),
            )
            .unwrap();
        assert!(
            tasks
                .queue_update(
                    &params.task_id,
                    &params.request_id,
                    proto::TaskQueueState::Waiting,
                    Some("late".into()),
                    None
                )
                .is_err()
        );
        assert!(
            tasks
                .dequeue(proto::TaskDequeueParams {
                    task_id: params.task_id.clone(),
                    request_id: params.request_id.clone()
                })
                .is_err()
        );
        assert_eq!(
            tasks.claim_launch(&params.task_id, &params.request_id, "unused", hash).unwrap(),
            (launch, false)
        );
    }

    #[test]
    fn keys_ignore_priority_tags_case_and_spacing_but_not_the_path() {
        assert_eq!(
            ledger_key("a.rs", "[P1] Divide  by Zero"),
            ledger_key("a.rs", "divide by zero")
        );
        assert_eq!(ledger_key("a.rs", " [P3]divide by zero "), "a.rs::divide by zero");
        assert_ne!(ledger_key("a.rs", "x"), ledger_key("b.rs", "x"));
        assert_eq!(ledger_key("a.rs", "[Px] odd"), "a.rs::[px] odd");
    }

    #[test]
    fn findings_open_resolve_and_reappear_across_reviews() {
        let a = finding("a.rs", "[P1] Alpha", true);
        let b = finding("a.rs", "[P2] Beta", false);
        let first = ledger(&[], "r1", &[a.clone(), b.clone(), a.clone()]);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|e| e.status == LedgerStatus::Open && e.seen == 1));
        let second = ledger(&first, "r2", &[b]);
        let status = |entries: &[LedgerEntry], key: &str| {
            entries.iter().find(|e| e.key.contains(key)).map(|e| (e.status, e.seen)).unwrap()
        };
        assert_eq!(status(&second, "alpha"), (LedgerStatus::Resolved, 1));
        assert_eq!(status(&second, "beta"), (LedgerStatus::Open, 2));
        let third = ledger(&second, "r3", &[finding("a.rs", "alpha", false)]);
        assert_eq!(status(&third, "alpha"), (LedgerStatus::Reappeared, 2));
        assert_eq!(status(&third, "beta"), (LedgerStatus::Resolved, 2));
        let alpha = third.iter().find(|e| e.key.contains("alpha")).unwrap();
        assert_eq!((alpha.first_review.as_str(), alpha.last_review.as_str()), ("r1", "r3"));
        assert!(!alpha.blocking, "as last reported");
        let fourth = ledger(&third, "r4", &[finding("a.rs", "alpha", true)]);
        assert_eq!(status(&fourth, "alpha"), (LedgerStatus::Reappeared, 3), "stays reappeared");
    }

    #[test]
    fn tasks_stored_before_the_loop_policy_load_with_its_defaults() {
        let old = r#"{"id":"task-1","title":"t","goal":"g","constraints":null,"criteria":[],
            "sessions":[],"status":"open","created_at_ms":1,"ledger":[{"key":"a.rs::x",
            "path":"a.rs","title":"x","status":"open","first_review":"r","last_review":"r",
            "seen":1,"blocking":true}]}"#;
        let task: Task = serde_json::from_str(old).unwrap();
        assert_eq!(task.fix_budget, proto::TASK_DEFAULT_FIX_BUDGET);
        assert!(task.fix_rounds.is_empty() && task.attention.is_none());
        assert_eq!(task.invalid_streak, 0);
        assert!(task.queue.is_none());
        assert!(task.ledger[0].dispute.is_none());
    }

    #[test]
    fn a_disputed_finding_stays_disputed_whether_reported_again_or_not() {
        let mut entries = ledger(&[], "r1", &[finding("a.rs", "Alpha", true)]);
        entries[0].status = LedgerStatus::Disputed;
        let again = ledger(&entries, "r2", &[finding("a.rs", "alpha", true)]);
        assert_eq!((again[0].status, again[0].seen), (LedgerStatus::Disputed, 2));
        let absent = ledger(&again, "r3", &[]);
        assert_eq!(absent[0].status, LedgerStatus::Disputed);
    }
}
