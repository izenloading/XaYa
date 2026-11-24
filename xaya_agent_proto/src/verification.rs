use serde::{Deserialize, Serialize};

pub const CHECKPOINT_CREATE: &str = "checkpoint.create";
pub const CHECKPOINT_GET: &str = "checkpoint.get";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCreateParams {
    pub session_id: String,
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointParams {
    pub checkpoint_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    Creating,
    Ready,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    pub command_id: String,
    pub status: CheckpointStatus,
    pub cwd: String,
    pub root: Option<String>,
    pub base_commit: Option<String>,
    pub tree: Option<String>,
    pub commit: Option<String>,
    pub git_ref: Option<String>,
    pub created_at_ms: u64,
    pub error: Option<String>,
}

pub const CHECK_RUN: &str = "check.run";
pub const CHECK_GET: &str = "check.get";
pub const CHECK_CLEANUP: &str = "check.cleanup";
pub const CHECK_CANCEL: &str = "check.cancel";
pub const CHECK_LOG: &str = "check.log";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRunParams {
    pub checkpoint_id: String,
    pub command_id: String,
    pub argv: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckParams {
    pub check_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckLogParams {
    pub check_id: String,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

pub const CHECK_BIND: &str = "check.bind";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckBindParams {
    pub check_id: String,
    pub command_id: String,
    /// Tracked file to corrupt; by default a non-test source file the checkpoint changed.
    #[serde(default)]
    pub path: Option<String>,
}

/// Whether anything proved that a passing check read the tree it names.
/// `CannotCheck` is never a pass; `NotChecked` is the honest default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    #[default]
    NotChecked,
    Running,
    Proved,
    Unbound,
    CannotCheck,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub status: BindingStatus,
    pub command_id: Option<String>,
    /// Paths corrupted, in order; for `Proved` the last one changed the outcome.
    pub mutated_paths: Vec<String>,
    /// Canary check IDs in order: the clean baseline first.
    pub runs: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryRole {
    Baseline,
    Mutated,
}

/// Marks a check that only exists as binding evidence for another check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canary {
    pub parent_check_id: String,
    pub role: CanaryRole,
    pub mutated_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    #[serde(default)]
    pub binding: Binding,
    #[serde(default)]
    pub canary: Option<Canary>,
    #[serde(default)]
    pub integration_cleanup_pending: bool,
    #[serde(default)]
    pub integration_cleanup_error: Option<String>,
    /// Integrated candidate receipt; freshness is checked through land.recheck.
    #[serde(default)]
    pub integration_preview_id: Option<String>,
    #[serde(default)]
    pub project_hash: Option<String>,
    #[serde(default)]
    pub steps: Vec<crate::CheckStepResult>,
    #[serde(default)]
    pub created_at_ms: u64,
    pub id: String,
    pub checkpoint_id: String,
    pub command_id: String,
    pub session_id: String,
    pub argv: Vec<String>,
    pub timeout_secs: u64,
    pub cwd: String,
    pub root: String,
    pub base_commit: String,
    pub commit: String,
    pub tree: String,
    pub status: CheckStatus,
    /// Conservative, sticky invalidation. A running check is never fresh.
    pub stale: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub log_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckLog {
    pub text: String,
    /// UTF-8 byte offset for the next request.
    pub next_offset: usize,
    pub complete: bool,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_single_command_receipts_remain_readable_without_project_fields() {
        let check: Check = serde_json::from_value(serde_json::json!({
            "id":"check", "session_id":"session", "checkpoint_id":"checkpoint", "command_id":"command",
            "argv":["true"], "timeout_secs":1, "root":"/project", "cwd":"/project", "base_commit":"base",
            "commit":"commit", "tree":"tree", "status":"succeeded", "stale":false,
            "exit_code":0,"duration_ms":10,"error":null,"log_truncated":false
        })).unwrap();
        assert!(check.project_hash.is_none());
        assert!(check.steps.is_empty());
        // Receipts from before binding proof load as never checked, not as proved.
        assert_eq!(check.binding, Binding::default());
        assert_eq!(check.binding.status, BindingStatus::NotChecked);
        assert!(check.canary.is_none());
        assert_eq!(check.created_at_ms, 0);
        assert_eq!(check.status, CheckStatus::Succeeded);
    }
}
