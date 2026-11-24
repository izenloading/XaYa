//! What a person sees to accept a task: the reviewed change, its verification and review, and
//! whether `task land` can take it now. Read-only.
use std::path::Path;
use std::sync::atomic::AtomicBool;

use xaya_agent_proto::{self as proto, ChangedFile, TaskAcceptance, TaskLandingState};

use crate::Result;
use crate::checkpoints::Git;

/// The diff shown is cut here, at a line end; the file list stays complete.
const MAX_DIFF: usize = 256 * 1024;

pub(crate) fn acceptance(
    task_id: &str,
    tasks: &crate::tasks::Tasks,
    reviews: &crate::reviews::Reviews,
    checks: &crate::checks::Checks,
    stopping: &AtomicBool,
) -> Result<TaskAcceptance> {
    let task = tasks.get(task_id)?;
    let review = reviews.list(Some(task_id), None).into_iter().next();
    let check = review.as_ref().and_then(|review| checks.get(review.check_id.as_ref()?).ok());
    let target_branch =
        task.landing.as_ref().map(|landing| landing.target_branch.clone()).or_else(|| {
            let workspace = task.queue.as_ref()?.workspace.as_ref()?;
            Some(workspace.base_branch.clone())
        });
    let (files, diff, diff_truncated) = match &review {
        Some(review) => changes(review, task.project.as_deref(), stopping)?,
        None => (Vec::new(), String::new(), false),
    };
    let blocked = blocked(&task, review.as_ref(), target_branch.is_some());
    Ok(TaskAcceptance { task, target_branch, review, check, files, diff, diff_truncated, blocked })
}

/// Why `task land` cannot take the task now (without naming a target), or none.
fn blocked(task: &proto::Task, review: Option<&proto::Review>, target: bool) -> Option<String> {
    match task.landing.as_ref().map(|landing| landing.state) {
        Some(TaskLandingState::Landing) => return Some("it is landing".into()),
        Some(TaskLandingState::Landed) => return Some("it has landed".into()),
        Some(TaskLandingState::Failed) | None => (),
    }
    if task.status != proto::TaskStatus::Open {
        return Some("the task is closed".into());
    }
    if task.drive.as_ref().is_some_and(|drive| drive.state.is_active()) {
        return Some("changes are under way; it will be reviewed again".into());
    }
    if let Err(gap) = crate::task_landing::approval(review) {
        return Some(gap.into());
    }
    (!target).then(|| "the task has no workspace base; land it with --target".into())
}

/// The reviewed change, read in its checkout, or in the repository once a landed task's
/// workspace is gone (the checkpoint's reference keeps its commits).
fn changes(
    review: &proto::Review,
    project: Option<&str>,
    stopping: &AtomicBool,
) -> Result<(Vec<ChangedFile>, String, bool)> {
    let root = match project {
        Some(project) if !Path::new(&review.root).exists() => project,
        _ => review.root.as_str(),
    };
    let git = Git::new(Path::new(root), stopping);
    let range = [review.base_commit.as_str(), review.commit.as_str()];
    let common = ["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--no-renames"];
    let numstat = git.run(&[&common[..], &["--numstat", "-z"], &range[..]].concat(), None)?;
    let files = numstat
        .split('\0')
        .filter_map(|entry| {
            let mut fields = entry.splitn(3, '\t');
            let (added, removed, path) = (fields.next()?, fields.next()?, fields.next()?);
            Some(ChangedFile {
                path: path.to_owned(),
                added: added.parse().ok(),
                removed: removed.parse().ok(),
            })
        })
        .collect();
    let output = git.output(&[&common[..], &range[..]].concat(), None)?;
    if !output.status.success() {
        return Err(crate::Error::Rpc {
            code: proto::ENGINE_ERROR,
            message: format!("Git failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
        });
    }
    let (diff, truncated) = cut(&output.stdout);
    Ok((files, diff, truncated))
}

/// The diff up to `MAX_DIFF` bytes, ending at a line end when cut.
fn cut(diff: &[u8]) -> (String, bool) {
    if diff.len() <= MAX_DIFF {
        return (String::from_utf8_lossy(diff).into_owned(), false);
    }
    let head = &diff[..MAX_DIFF];
    let end = head.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    (String::from_utf8_lossy(&head[..end]).into_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_diff_is_cut_at_a_line_end() {
        assert_eq!(cut(b"a\nb\n"), ("a\nb\n".into(), false));
        let long = "line\n".repeat(MAX_DIFF / 5 + 10);
        let (diff, truncated) = cut(long.as_bytes());
        assert!(truncated);
        assert!(diff.len() <= MAX_DIFF && diff.ends_with('\n'));
    }
}
