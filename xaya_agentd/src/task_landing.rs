//! Landing a task's approved change, one durable step at a time: preview the squash onto the
//! target branch, verify it there, update the branch, then accept the task. Each step is the
//! `land` operation a person would run; its command ID derives from the landing request, so a
//! step retaken after a restart queries the same operation instead of starting another.
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use xaya_agent_proto::{
    self as proto, LandStatus, TaskLandingRecord, TaskWorkspaceCleanup, TaskWorkspaceOutcome,
};

use crate::Result;
use crate::drive::Call;

fn get<T: DeserializeOwned>(
    call: &mut impl Call,
    method: &str,
    params: impl Serialize,
) -> Result<T> {
    Ok(serde_json::from_value(call.call(method, serde_json::to_value(params)?)?)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// The landing preview now exists; record it and take the next step.
    Previewed(String),
    /// Nothing to do until the operation in flight finishes.
    Wait(String),
    Landed,
    /// A person has to look.
    Failed(String),
}

/// The preview's command ID: one per task and landing request, within the 64-byte limit.
fn command_id(task_id: &str, request_id: &str) -> String {
    let digest = Sha256::digest(format!("{task_id}\n{request_id}").as_bytes());
    let hex: String = digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect();
    format!("task-land-{hex}")
}

/// The task's latest review as something to land: a current approval of a verified checkpoint
/// (the same review dependencies are judged by), with its verification; else what is missing.
pub(crate) fn approval(
    latest: Option<&proto::Review>,
) -> std::result::Result<(&proto::Review, &str), &'static str> {
    use proto::{ReviewStatus, ReviewVerdict};
    let review = latest.ok_or("the task has no review yet")?;
    match (review.status, review.verdict) {
        (ReviewStatus::Running, _) => return Err("its review is still running"),
        (ReviewStatus::Completed, Some(ReviewVerdict::Approved)) => (),
        (ReviewStatus::Completed, Some(ReviewVerdict::NeedsFix)) => {
            return Err("its latest review asks for changes");
        },
        (ReviewStatus::Completed, _) => return Err("its latest review did not approve"),
        (ReviewStatus::Failed | ReviewStatus::Cancelled | ReviewStatus::Interrupted, _) => {
            return Err("its latest review did not finish");
        },
    }
    if review.stale {
        return Err("the checkout changed after the approval");
    }
    let check = review.check_id.as_deref().ok_or("the approval has no verification")?;
    Ok((review, check))
}

/// Take one step of `landing` for task `task_id`. An error is a failure a person has to see.
pub(crate) fn step(
    call: &mut impl Call,
    task_id: &str,
    landing: &TaskLandingRecord,
) -> Result<Step> {
    let task: proto::Task =
        get(call, proto::TASK_GET, proto::TaskParams { task_id: task_id.into() })?;
    let Some(preview_id) = &landing.preview_id else {
        if task.status != proto::TaskStatus::Open {
            return Ok(Step::Failed("the task was closed before it landed".into()));
        }
        let reviews: Vec<proto::Review> = get(call, proto::REVIEW_LIST, proto::ReviewListParams {
            task_id: Some(task_id.into()),
            session_id: None,
        })?;
        let (checkpoint_id, check_id) = match approval(reviews.first()) {
            Ok((review, check)) => (review.checkpoint_id.clone(), check.to_owned()),
            Err(gap) => return Ok(Step::Failed(format!("not landable: {gap}"))),
        };
        let preview: proto::LandPreview =
            get(call, proto::LAND_PREVIEW, proto::LandPreviewParams {
                checkpoint_id,
                check_id,
                target_branch: landing.target_branch.clone(),
                command_id: command_id(task_id, &landing.request_id),
            })?;
        return Ok(Step::Previewed(preview.id));
    };
    let params = || proto::LandParams { preview_id: preview_id.clone() };
    let preview: proto::LandPreview = get(call, proto::LAND_GET, params())?;
    let failed = |preview: &proto::LandPreview, what: &str| {
        let detail = preview.error.clone().unwrap_or_else(|| format!("{:?}", preview.status));
        Step::Failed(format!("{what} ({}): {detail}", preview.id))
    };
    if task.status != proto::TaskStatus::Open && preview.status != LandStatus::Landed {
        return Ok(Step::Failed("the task was closed before it landed".into()));
    }
    let target = &landing.target_branch;
    Ok(match preview.status {
        LandStatus::Landed => accept(call, task_id, &task)?,
        LandStatus::Creating => Step::Wait(format!("building the candidate for {target}")),
        LandStatus::Verifying => Step::Wait(format!("verifying the candidate on {target}")),
        LandStatus::Ready if !preview.stale && preview.integration_verified => {
            let landed: proto::LandPreview = get(call, proto::LAND_RUN, params())?;
            if landed.status == LandStatus::Landed {
                accept(call, task_id, &task)?
            } else {
                failed(&landed, "the branch was not updated; see land reconcile")
            }
        },
        LandStatus::Ready if !preview.stale && preview.integration_check_id.is_none() => {
            let _: proto::LandPreview = get(call, proto::LAND_VERIFY, params())?;
            Step::Wait(format!("verifying the candidate on {target}"))
        },
        LandStatus::Conflict => failed(&preview, "the change conflicts with the target"),
        _ => failed(&preview, "the landing stopped"),
    })
}

/// The change is on the branch: accept the task, which also lets its dependents start.
fn accept(call: &mut impl Call, task_id: &str, task: &proto::Task) -> Result<Step> {
    if task.status == proto::TaskStatus::Open {
        let _: proto::Task = get(call, proto::TASK_CLOSE, proto::TaskCloseParams {
            task_id: task_id.into(),
            status: proto::TaskStatus::Accepted,
        })?;
    }
    Ok(Step::Landed)
}

/// The next step for a landed task's workspace, at `now_ms`: `Removing` is recorded before
/// anything is deleted, then [`remove`] finishes it. A failure to read or stop something keeps
/// the workspace for a person.
pub(crate) fn inspect(
    call: &mut impl Call,
    task_id: &str,
    landing: &TaskLandingRecord,
    stopping: &AtomicBool,
    now_ms: u64,
) -> TaskWorkspaceCleanup {
    let outcome = |outcome, path: Option<&str>, reason: Option<String>| TaskWorkspaceCleanup {
        outcome,
        path: path.map(str::to_owned),
        reason,
        at_ms: now_ms,
    };
    let task: proto::Task = match get(call, proto::TASK_GET, proto::TaskParams {
        task_id: task_id.into(),
    }) {
        Ok(task) => task,
        Err(error) => return outcome(TaskWorkspaceOutcome::Kept, None, Some(error.to_string())),
    };
    let Some(path) =
        task.project.as_deref().and_then(|project| crate::workspaces::path(project, task_id).ok())
    else {
        return outcome(TaskWorkspaceOutcome::Absent, None, None);
    };
    if std::fs::symlink_metadata(&path).is_err() {
        return outcome(TaskWorkspaceOutcome::Absent, Some(&path), None);
    }
    match keep_reason(call, &task, landing, &path, stopping) {
        Ok(()) => outcome(TaskWorkspaceOutcome::Removing, Some(&path), None),
        Err(reason) => outcome(TaskWorkspaceOutcome::Kept, Some(&path), Some(reason)),
    }
}

/// Why the workspace at `path` must stay, if it must; the task's own live sessions in it are
/// stopped first.
fn keep_reason(
    call: &mut impl Call,
    task: &proto::Task,
    landing: &TaskLandingRecord,
    path: &str,
    stopping: &AtomicBool,
) -> std::result::Result<(), String> {
    let project = task.project.as_deref().unwrap_or_default();
    if !crate::workspaces::belongs(project, path, stopping) {
        return Err("it is not a checkout of the task's repository".into());
    }
    let inside =
        |cwd: &str| cwd == path || cwd.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'));
    let sessions: Vec<proto::Session> =
        get(call, proto::SESSION_LIST, serde_json::json!({})).map_err(|e| e.to_string())?;
    let live = sessions.into_iter().filter(|session| session.state.is_active());
    for session in live.filter(|session| inside(&session.cwd)) {
        if !task.sessions.contains(&session.id) {
            return Err(format!("session {}, not this task's, works in it", session.id));
        }
        let _: serde_json::Value =
            get(call, proto::SESSION_STOP, proto::SessionParams { session_id: session.id.clone() })
                .map_err(|error| format!("could not stop session {}: {error}", session.id))?;
    }
    let preview_id = landing.preview_id.clone().ok_or("the landing has no preview")?;
    let preview: proto::LandPreview =
        get(call, proto::LAND_GET, proto::LandParams { preview_id }).map_err(|e| e.to_string())?;
    let changed = || {
        format!(
            "it changed after the landed checkpoint; remove it when done with `git worktree \
             remove --force {path}`"
        )
    };
    let (head, tree) = crate::checkpoints::current_tree(Path::new(path), stopping)
        .map_err(|error| format!("could not read it ({error}); {}", changed()))?;
    if head != preview.checkpoint_base || tree != preview.checkpoint_tree {
        return Err(changed());
    }
    Ok(())
}

/// Delete the workspace at `path` of the repository `project` (a canonical Git common
/// directory), whatever an interrupted removal left of it.
pub(crate) fn remove(project: &str, path: &str, stopping: &AtomicBool) -> Result<()> {
    let git = crate::checkpoints::Git::new(Path::new(project), stopping);
    if std::fs::symlink_metadata(path).is_ok() {
        // Twice forced: also a locked worktree. A partly deleted one is no longer a worktree.
        let _ = git.output(&["worktree", "remove", "--force", "--force", "--", path], None)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    git.run(&["worktree", "prune"], None)?;
    // The `<repository>.xaya` directory goes once it holds no other workspace.
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use serde_json::{Value, json};

    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    /// A repository with a task workspace holding one uncommitted change, and the landing
    /// record of that change's checkpoint.
    struct Fixture {
        _temp: tempfile::TempDir,
        project: String,
        path: String,
        landing: TaskLandingRecord,
        base: String,
        tree: String,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"]);
        let stopping = AtomicBool::new(false);
        let project = crate::tasks::repository(repo.to_str().unwrap(), &stopping).unwrap();
        let path = crate::workspaces::path(&project, "task-1").unwrap();
        crate::workspaces::prepare(&project, &path, &git(&repo, &["rev-parse", "HEAD"]), &stopping)
            .unwrap();
        std::fs::write(Path::new(&path).join("a.txt"), "changed\n").unwrap();
        let (base, tree) = crate::checkpoints::current_tree(Path::new(&path), &stopping).unwrap();
        let landing = TaskLandingRecord {
            request_id: "land-1".into(),
            target_branch: "main".into(),
            state: proto::TaskLandingState::Landed,
            reason: None,
            preview_id: Some("preview-1".into()),
            started_at_ms: 1,
            updated_at_ms: 1,
            workspace: None,
        };
        Fixture { _temp: temp, project, path, landing, base, tree }
    }

    /// Answers the calls cleanup makes, and records them.
    struct Service<'a> {
        fixture: &'a Fixture,
        sessions: Vec<(&'static str, String)>,
        calls: Vec<(String, Value)>,
    }

    impl Call for Service<'_> {
        fn call(&mut self, method: &str, params: Value) -> Result<Value> {
            self.calls.push((method.into(), params));
            let fixture = self.fixture;
            Ok(match method {
                proto::TASK_GET => json!({
                    "id":"task-1", "title":"t", "goal":"g", "constraints":null, "criteria":[],
                    "sessions":["own"], "status":"accepted", "created_at_ms":1,
                    "project":fixture.project,
                }),
                proto::SESSION_LIST => self
                    .sessions
                    .iter()
                    .map(|(id, cwd)| {
                        json!({"id":id, "engine":"claude", "cwd":cwd, "sandbox":"read-only",
                            "state":"ready", "native_thread_id":null, "turn_id":null,
                            "attached":false, "backend_pid":null, "last_error":null})
                    })
                    .collect(),
                proto::SESSION_STOP => Value::Null,
                proto::LAND_GET => json!({
                    "id":"preview-1", "checkpoint_id":"cp", "check_id":"check",
                    "command_id":"c", "session_id":"own", "source_root":fixture.path,
                    "checkpoint_commit":"x", "checkpoint_tree":fixture.tree,
                    "checkpoint_base":fixture.base, "checkpoint_ref":"refs/xaya/checkpoints/cp",
                    "check_project_hash":null, "target_ref":"refs/heads/main",
                    "target_head":null, "target_worktree":null, "merge_base":null,
                    "candidate_tree":null, "candidate_commit":null, "candidate_ref":null,
                    "diff_stat":null, "details":null, "output_truncated":false,
                    "status":"landed", "stale":false, "integration_verified":true,
                    "created_at_ms":1, "rechecked_at_ms":null, "error":null,
                }),
                _ => panic!("unexpected call {method}"),
            })
        }
    }

    fn inspected(
        fixture: &Fixture,
        sessions: Vec<(&'static str, String)>,
    ) -> (TaskWorkspaceCleanup, Vec<String>) {
        let mut service = Service { fixture, sessions, calls: Vec::new() };
        let stopping = AtomicBool::new(false);
        let cleanup = inspect(&mut service, "task-1", &fixture.landing, &stopping, 7);
        let stopped = service
            .calls
            .into_iter()
            .filter(|(method, _)| method == proto::SESSION_STOP)
            .map(|(_, params)| params["session_id"].as_str().unwrap().to_owned())
            .collect();
        (cleanup, stopped)
    }

    #[test]
    fn a_workspace_at_the_landed_checkpoint_is_removed_after_its_sessions_stop() {
        let fixture = fixture();
        // Ignored output does not count as a change.
        std::fs::create_dir(Path::new(&fixture.path).join("build")).unwrap();
        std::fs::write(Path::new(&fixture.path).join("build/out"), "x").unwrap();
        let inner = format!("{}/src", fixture.path);
        let elsewhere = "/elsewhere".to_owned();
        let (cleanup, stopped) = inspected(&fixture, vec![("own", inner), ("other", elsewhere)]);
        assert_eq!(cleanup.outcome, TaskWorkspaceOutcome::Removing, "{cleanup:?}");
        assert_eq!(cleanup.path.as_deref(), Some(fixture.path.as_str()));
        assert_eq!(stopped, ["own"], "only the task's sessions in the workspace stop");

        remove(&fixture.project, &fixture.path, &AtomicBool::new(false)).unwrap();
        assert!(!Path::new(&fixture.path).exists());
        assert!(!Path::new(&fixture.path).parent().unwrap().exists(), "the empty .xaya goes");
        let repo = Path::new(&fixture.project).parent().unwrap();
        let worktrees = git(repo, &["worktree", "list", "--porcelain"]);
        assert!(!worktrees.contains(&fixture.path), "{worktrees}");
        // Gone already: nothing left to look at.
        let (cleanup, _) = inspected(&fixture, Vec::new());
        assert_eq!(cleanup.outcome, TaskWorkspaceOutcome::Absent);
    }

    #[test]
    fn a_workspace_changed_after_landing_or_used_outside_the_task_is_kept() {
        let fixture = fixture();
        let (cleanup, stopped) = inspected(&fixture, vec![("someone", fixture.path.clone())]);
        assert_eq!(cleanup.outcome, TaskWorkspaceOutcome::Kept);
        assert!(cleanup.reason.unwrap().contains("someone"));
        assert!(stopped.is_empty(), "a session outside the task keeps running");

        std::fs::write(Path::new(&fixture.path).join("b.txt"), "later\n").unwrap();
        let (cleanup, _) = inspected(&fixture, Vec::new());
        assert_eq!(cleanup.outcome, TaskWorkspaceOutcome::Kept);
        assert!(cleanup.reason.unwrap().contains("changed after the landed checkpoint"));
        assert!(Path::new(&fixture.path).join("b.txt").exists());
    }

    #[test]
    fn an_interrupted_removal_is_finished() {
        let fixture = fixture();
        // Partly deleted: no longer a worktree Git can remove.
        std::fs::remove_file(Path::new(&fixture.path).join(".git")).unwrap();
        remove(&fixture.project, &fixture.path, &AtomicBool::new(false)).unwrap();
        assert!(!Path::new(&fixture.path).exists());
        let repo = Path::new(&fixture.project).parent().unwrap();
        let worktrees = git(repo, &["worktree", "list", "--porcelain"]);
        assert!(!worktrees.contains(&fixture.path), "pruned: {worktrees}");
        remove(&fixture.project, &fixture.path, &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn a_preview_command_id_is_stable_per_request_and_fits() {
        let id = command_id(&format!("task-{}", "f".repeat(32)), &"r".repeat(64));
        assert!(id.len() <= 64, "{id}");
        assert_eq!(id, command_id(&format!("task-{}", "f".repeat(32)), &"r".repeat(64)));
        assert_ne!(command_id("task-1", "a"), command_id("task-1", "b"));
        assert_ne!(command_id("task-1", "a"), command_id("task-2", "a"));
    }
}
