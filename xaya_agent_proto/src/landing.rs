use serde::{Deserialize, Serialize};

pub const LAND_PREVIEW: &str = "land.preview";
pub const LAND_GET: &str = "land.get";
pub const LAND_VERIFY: &str = "land.verify";
pub const LAND_RECONCILE: &str = "land.reconcile";
pub const LAND_RUN: &str = "land.run";
pub const LAND_RECHECK: &str = "land.recheck";
pub const LAND_RESUME: &str = "land.resume";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandPreviewParams {
    pub checkpoint_id: String,
    pub check_id: String,
    pub target_branch: String,
    pub command_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandParams {
    pub preview_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandStatus {
    Creating,
    Verifying,
    Landing,
    Landed,
    Ready,
    Conflict,
    Blocked,
    Failed,
    Interrupted,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandOutcome {
    Applied,
    NotApplied,
    NeedsAttention,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandPreview {
    pub id: String,
    pub checkpoint_id: String,
    pub check_id: String,
    pub command_id: String,
    pub session_id: String,
    pub source_root: String,
    pub checkpoint_commit: String,
    pub checkpoint_tree: String,
    pub checkpoint_base: String,
    pub checkpoint_ref: String,
    pub check_project_hash: Option<String>,
    /// The project required a proved binding when this preview was inspected; never relaxed.
    #[serde(default)]
    pub binding_required: bool,
    pub target_ref: String,
    pub target_head: Option<String>,
    pub target_worktree: Option<String>,
    pub merge_base: Option<String>,
    pub candidate_tree: Option<String>,
    pub candidate_commit: Option<String>,
    pub candidate_ref: Option<String>,
    pub diff_stat: Option<String>,
    pub details: Option<String>,
    pub output_truncated: bool,
    pub status: LandStatus,
    /// Sampled effect of a durable landing attempt; never causes replay.
    #[serde(default)]
    pub landing_outcome: Option<LandOutcome>,
    pub stale: bool,
    #[serde(default)]
    pub integration_check_id: Option<String>,
    /// True only after successful verification of this exact candidate.
    pub integration_verified: bool,
    pub created_at_ms: u64,
    pub rechecked_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandRecheck {
    pub preview: LandPreview,
    /// These preconditions were sampled again; land.run additionally requires a successful
    /// integrated check.
    pub valid: bool,
    pub reason: Option<String>,
}
