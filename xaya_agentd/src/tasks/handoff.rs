//! The handoff bundle a reviewer receives for a task: built by code from the task record and the
//! reviewed range, never summarized by a model, and without earlier attempts or the
//! implementer's conversation (design §11.1, §11.4).
use xaya_agent_proto::{self as proto, Handoff, HandoffFile, Task};

use super::sha256;
use crate::checkpoints::Git;
use crate::{Error, Result};

const MAX_FILES: usize = 40;

fn clip(text: &str, limit: usize, truncated: &mut bool) -> String {
    if text.chars().count() > limit {
        *truncated = true;
        text.chars().take(limit).collect()
    } else {
        text.to_owned()
    }
}

/// Changed paths of `base..commit` with their blob in `commit` (`None` when deleted), from
/// `git diff-tree -r -z --raw`: `:<mode> <mode> <old> <new> <status>\0<path>\0`.
pub(crate) fn changed_files(raw: &str) -> Vec<HandoffFile> {
    let mut fields = raw.split('\0');
    let mut files = Vec::new();
    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(':') else { continue };
        let Some(path) = fields.next() else { break };
        let parts: Vec<&str> = meta.split(' ').collect();
        let (Some(new), Some(status)) = (parts.get(3), parts.get(4)) else { continue };
        let deleted = status.starts_with('D') || new.bytes().all(|b| b == b'0');
        files.push(HandoffFile {
            path: path.to_owned(),
            blob: (!deleted).then(|| (*new).to_owned()),
        });
    }
    files
}

/// The bundle for reviewing `base..commit` of the repository at `git`'s root against `task`.
pub(crate) fn build(task: &Task, git: &Git<'_>, base: &str, commit: &str) -> Result<Handoff> {
    if task.criteria.iter().any(|criterion| sha256(&criterion.text) != criterion.hash) {
        return Err(Error::Rpc {
            code: proto::CONFLICT,
            message: "STALE_BASELINE: a criterion no longer matches its hash".into(),
        });
    }
    let mut truncated = false;
    let raw = git.output(
        &["diff-tree", "-r", "-z", "--raw", "--no-renames", "--no-ext-diff", base, commit],
        None,
    );
    let mut files = match raw {
        Ok(output) if output.status.success() => {
            changed_files(&String::from_utf8_lossy(&output.stdout))
        },
        // Too many changes for the bounded Git output: the bundle says the list is incomplete.
        Err(error) if error.to_string().contains("exceeds") => {
            truncated = true;
            Vec::new()
        },
        Ok(output) => {
            return Err(Error::Rpc {
                code: proto::ENGINE_ERROR,
                message: format!("Git failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
            });
        },
        Err(error) => return Err(error),
    };
    if files.len() > MAX_FILES {
        files.truncate(MAX_FILES);
        truncated = true;
    }
    let mut handoff = Handoff {
        task_id: task.id.clone(),
        title: task.title.clone(),
        goal: clip(&task.goal, proto::TASK_MAX_GOAL, &mut truncated),
        constraints: task
            .constraints
            .as_deref()
            .map(|c| clip(c, proto::TASK_MAX_CONSTRAINTS, &mut truncated)),
        criteria: task.criteria.clone(),
        files,
        truncated,
        hash: String::new(),
    };
    handoff.hash = sha256(&serde_json::to_string(&handoff)?);
    Ok(handoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_diff_tree_lists_paths_with_their_new_blob() {
        let raw = ":100644 100644 aaaa bbbb M\0src/a.rs\0:000000 100644 0000 cccc A\0src/new \
                   file.rs\0:100644 000000 dddd 0000 D\0gone.rs\0";
        assert_eq!(changed_files(raw), vec![
            HandoffFile { path: "src/a.rs".into(), blob: Some("bbbb".into()) },
            HandoffFile { path: "src/new file.rs".into(), blob: Some("cccc".into()) },
            HandoffFile { path: "gone.rs".into(), blob: None },
        ]);
        assert!(changed_files("").is_empty());
    }

    #[test]
    fn clipping_marks_the_bundle_truncated() {
        let mut truncated = false;
        assert_eq!(clip("short", 10, &mut truncated), "short");
        assert!(!truncated);
        assert_eq!(clip("ééééé", 3, &mut truncated), "ééé");
        assert!(truncated);
    }
}
