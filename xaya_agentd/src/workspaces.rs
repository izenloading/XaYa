//! Task workspaces: a detached Git worktree the service creates for one task, beside the
//! project at `<parent>/<repository>.xaya/<task ID>`. It holds no branch, so creating one never
//! collides with the user's branches; checkpoints and landings work from any checkout.
//!
//! A workspace is created only after every gate before the claim has passed, so a task that
//! waits for capacity or a prerequisite leaves nothing on disk. A crash between creating it and
//! claiming the task leaves a clean detached worktree at the task's path, which the next attempt
//! adopts. Anything else found at that path is never touched.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use xaya_agent_proto as proto;

use crate::checkpoints::Git;
use crate::{Error, Result};

fn conflict(message: String) -> Error {
    Error::Rpc { code: proto::CONFLICT, message }
}

fn utf8(path: PathBuf) -> Result<String> {
    path.into_os_string().into_string().map_err(|_| Error::Protocol("workspace path must be UTF-8"))
}

/// The task's workspace for the canonical Git common directory `project`.
pub(crate) fn path(project: &str, task_id: &str) -> Result<String> {
    if task_id.is_empty()
        || task_id.len() > 64
        || !task_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::Protocol("task ID cannot name a workspace"));
    }
    let common = Path::new(project);
    // A checkout's `.git`, or a bare repository's own directory.
    let root = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent()
    } else {
        Some(common)
    };
    let name = root
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".git").unwrap_or(name))
        .filter(|name| !name.is_empty());
    let (Some(parent), Some(name)) = (root.and_then(Path::parent), name) else {
        return Err(conflict(format!("cannot place a workspace beside {project}")));
    };
    let path = parent.join(format!("{name}.xaya")).join(task_id);
    if path.as_os_str().len() > 1024 {
        return Err(conflict("workspace path exceeds 1024 bytes".into()));
    }
    utf8(path)
}

/// An existing canonical directory that stands in for a workspace not created yet, so an
/// engine launch can be validated before anything is written.
pub(crate) fn stand_in(path: &str) -> Result<String> {
    let parent = Path::new(path)
        .parent()
        .and_then(Path::parent)
        .ok_or(Error::Protocol("workspace path has no parent"))?;
    utf8(parent.to_path_buf())
}

/// Reject a base that cannot name a local branch before anything is stored.
pub(crate) fn validate_base(workspace: &proto::TaskWorkspace) -> Result<String> {
    crate::landings::target_ref(&workspace.base_branch).map_err(|_| Error::Rpc {
        code: proto::INVALID_PARAMS,
        message: "workspace base must name a local branch (<=256 bytes)".into(),
    })
}

/// The branch checked out in `cwd`, as a workspace base.
pub(crate) fn current_branch(cwd: &str, stopping: &AtomicBool) -> Result<String> {
    let output =
        Git::new(Path::new(cwd), stopping).output(&["symbolic-ref", "--quiet", "HEAD"], None)?;
    let branch = String::from_utf8(output.stdout).unwrap_or_default();
    match branch.trim().strip_prefix("refs/heads/") {
        Some(name) if output.status.success() && !name.is_empty() => Ok(name.to_owned()),
        _ => Err(conflict(
            "the checkout is not on a branch; check one out or name the base branch".into(),
        )),
    }
}

/// The commit at the tip of the base branch.
pub(crate) fn base_tip(
    project: &str,
    workspace: &proto::TaskWorkspace,
    stopping: &AtomicBool,
) -> Result<String> {
    let reference = validate_base(workspace)?;
    let git = Git::new(Path::new(project), stopping);
    let output = git.output(
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
        None,
    )?;
    if !output.status.success() || git.output(&["check-ref-format", &reference], None).is_err() {
        return Err(conflict(format!(
            "workspace base branch {} does not exist",
            workspace.base_branch
        )));
    }
    String::from_utf8(output.stdout)
        .map(|commit| commit.trim().to_owned())
        .map_err(|_| Error::Protocol("Git output must be UTF-8"))
}

/// Create the workspace at `commit`, or adopt a clean detached worktree of the same project
/// already at `path` (left by an attempt that stopped before claiming the task).
pub(crate) fn prepare(
    project: &str,
    path: &str,
    commit: &str,
    stopping: &AtomicBool,
) -> Result<()> {
    let target = Path::new(path);
    match fs::symlink_metadata(target) {
        Ok(_) => adopt(project, path, stopping),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            Git::new(Path::new(project), stopping)
                .run(&["worktree", "add", "--detach", "--", path, commit], None)
                .map_err(|error| {
                    conflict(format!("could not create the task workspace: {error}"))
                })?;
            adopt(project, path, stopping)
        },
        Err(error) => Err(error.into()),
    }
}

/// Whether `path` is a real directory at the top of a checkout of `project`.
pub(crate) fn belongs(project: &str, path: &str, stopping: &AtomicBool) -> bool {
    let target = Path::new(path);
    fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir())
        && target.canonicalize().is_ok_and(|canonical| canonical == target)
        && crate::tasks::repository(path, stopping).ok().as_deref() == Some(project)
        && Git::new(target, stopping).run(&["rev-parse", "--show-toplevel"], None).ok().as_deref()
            == Some(path)
}

fn adopt(project: &str, path: &str, stopping: &AtomicBool) -> Result<()> {
    let refuse = || {
        conflict(format!(
            "workspace path {path} holds something other than a clean detached worktree of this \
             project; move it away, or dequeue and reset the task"
        ))
    };
    if !belongs(project, path, stopping) {
        return Err(refuse());
    }
    let git = Git::new(Path::new(path), stopping);
    // Detached: `symbolic-ref` reports exit status 1 when HEAD is not a branch.
    if git.output(&["symbolic-ref", "--quiet", "HEAD"], None)?.status.code() != Some(1) {
        return Err(refuse());
    }
    let status = git.output(&["status", "--porcelain=v1", "-z", "--untracked-files=all"], None)?;
    if !status.status.success() || !status.stdout.is_empty() {
        return Err(refuse());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_sit_beside_the_checkout_or_bare_repository() {
        assert_eq!(path("/src/app/.git", "task-1").unwrap(), "/src/app.xaya/task-1");
        assert_eq!(path("/src/app.git", "task-1").unwrap(), "/src/app.xaya/task-1");
        assert_eq!(path("/src/app", "task-1").unwrap(), "/src/app.xaya/task-1");
        assert_eq!(stand_in("/src/app.xaya/task-1").unwrap(), "/src");
        for id in ["", "../x", "a/b", "task 1", &"x".repeat(65)] {
            assert!(path("/src/app/.git", id).is_err(), "{id}");
        }
        for project in ["/.git", "/"] {
            assert!(path(project, "task-1").is_err(), "{project}");
        }
    }

    #[test]
    fn a_base_must_name_a_local_branch() {
        let base = |name: &str| proto::TaskWorkspace { base_branch: name.into() };
        assert_eq!(validate_base(&base("main")).unwrap(), "refs/heads/main");
        assert_eq!(validate_base(&base("refs/heads/x")).unwrap(), "refs/heads/x");
        for name in ["", "-x", "refs/tags/v1", "a\nb"] {
            assert!(validate_base(&base(name)).is_err(), "{name:?}");
        }
    }
}
