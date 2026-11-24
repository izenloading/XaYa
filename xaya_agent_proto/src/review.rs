//! L2 review of a checkpoint by a headless engine. The engine's structured review is one input;
//! the verdict is decided by the service.
use serde::{Deserialize, Serialize};

pub const REVIEW_START: &str = "review.start";
pub const REVIEW_GET: &str = "review.get";
pub const REVIEW_CANCEL: &str = "review.cancel";
/// Reviews, newest first; only a task's or a session's when given.
pub const REVIEW_LIST: &str = "review.list";
/// Send a needs-fix review's findings back to the implementing session, once.
pub const REVIEW_SEND: &str = "review.send";
/// Reject one finding of a task review, with a reason.
pub const REVIEW_DISPUTE: &str = "review.dispute";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDisputeParams {
    pub review_id: String,
    /// The finding's number in the review, from 1.
    pub finding: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSendParams {
    pub review_id: String,
    pub command_id: String,
}

/// A review's findings were sent back to its session with this program-send command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentFix {
    pub command_id: String,
    pub session_id: String,
    pub at_ms: u64,
}

/// Default wall-clock budget for one review.
pub const REVIEW_DEFAULT_TIMEOUT_SECS: u64 = 900;

fn default_timeout() -> u64 {
    REVIEW_DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEngine {
    Codex,
    Claude,
}

impl ReviewEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_reviewer: Option<ReviewerSpec>,
    pub checkpoint_id: String,
    pub command_id: String,
    pub engine: ReviewEngine,
    /// Absolute engine executable, resolved by the caller.
    pub executable: String,
    /// The caller's CODEX_HOME or CLAUDE_CONFIG_DIR, only if it set one; `None` leaves the
    /// variable unset so the engine uses its own default (and, for Claude, its Keychain login).
    #[serde(default)]
    pub config_dir: Option<String>,
    /// A verification of the same checkpoint; without a passing one nothing is approved.
    #[serde(default)]
    pub check_id: Option<String>,
    /// An ancestor of the checkpoint commit; by default the checkpoint's own base.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// An open task that includes the checkpoint's session: the reviewer gets its handoff
    /// bundle and judges each acceptance criterion.
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerSpec {
    pub engine: ReviewEngine,
    pub executable: String,
    #[serde(default)]
    pub config_dir: Option<String>,
}

/// Token counters as reported by the engine. Codex input includes its cache hits. Claude
/// reports uncached input, cache reads and cache writes as separate counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerResult {
    pub engine: ReviewEngine,
    pub verdict: Option<ReviewVerdict>,
    pub native_session_id: Option<String>,
    pub usage: Option<ReviewUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewListParams {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewParams {
    pub review_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Running,
    /// The reviewer finished; `verdict` says what it means.
    Completed,
    /// The reviewer could not be run to an answer (start failure, exit, timeout).
    Failed,
    Cancelled,
    Interrupted,
}

/// Only `Approved` passes. `Invalid` means the review itself cannot be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewVerdict {
    Approved,
    NeedsFix,
    Blocked,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub title: String,
    pub body: String,
    /// 0 (P0) to 3 (P3); absent when the reviewer gave none.
    pub priority: Option<u8>,
    pub confidence: f64,
    /// Relative to the repository root.
    pub path: String,
    pub start: u32,
    pub end: u32,
    /// P0/P1 with confidence of at least 0.5: needs a fix before approval.
    pub blocking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedFinding {
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub checkpoint_id: String,
    pub command_id: String,
    pub session_id: String,
    pub engine: ReviewEngine,
    /// The reviewer runs the same engine as the implementing session.
    pub same_engine: bool,
    pub executable: String,
    pub config_dir: Option<String>,
    /// Source checkout root and the checkpoint's cwd, for freshness.
    pub root: String,
    pub cwd: String,
    pub base_commit: String,
    pub commit: String,
    pub tree: String,
    pub check_id: Option<String>,
    pub timeout_secs: u64,
    pub status: ReviewStatus,
    pub verdict: Option<ReviewVerdict>,
    /// Why the service decided the verdict it did.
    pub reasons: Vec<String>,
    pub findings: Vec<ReviewFinding>,
    pub dropped: Vec<DroppedFinding>,
    pub overall_correct: Option<bool>,
    pub overall_explanation: Option<String>,
    pub diff_truncated: bool,
    /// The engine's own session or thread ID, to inspect natively.
    pub native_session_id: Option<String>,
    /// The checkout no longer matches the reviewed checkpoint; set once, never cleared.
    pub stale: bool,
    pub created_at_ms: u64,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub handoff: Option<crate::Handoff>,
    /// The reviewer's result for each acceptance criterion, as kept by the service.
    #[serde(default)]
    pub criteria: Vec<crate::CriterionResult>,
    #[serde(default)]
    pub sent: Option<SentFix>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_reviewer: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<ReviewerResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_and_defaults_have_stable_wire_names() {
        assert_eq!(serde_json::to_value(ReviewVerdict::NeedsFix).unwrap(), "needs-fix");
        assert_eq!(serde_json::to_value(ReviewEngine::Claude).unwrap(), "claude");
        let params: ReviewStartParams = serde_json::from_value(serde_json::json!({
            "checkpoint_id": "cp", "command_id": "c", "engine": "codex",
            "executable": "/bin/codex",
        }))
        .unwrap();
        assert_eq!(params.timeout_secs, REVIEW_DEFAULT_TIMEOUT_SECS);
        assert!(params.check_id.is_none() && params.base.is_none() && params.config_dir.is_none());
    }
}
