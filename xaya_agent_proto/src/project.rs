use serde::{Deserialize, Serialize};

pub const PROJECT_GET: &str = "project.get";
pub const PROJECT_TRUST: &str = "project.trust";
pub const CHECK_PROJECT: &str = "check.project";
pub const CHECK_LATEST: &str = "check.latest";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectParams {
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTrustParams {
    pub cwd: String,
    pub config_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyStep {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub root: String,
    pub config_path: String,
    pub config_hash: String,
    pub trusted: bool,
    pub timeout_secs: u64,
    pub steps: Vec<VerifyStep>,
    /// Landing requires the source check's binding to be proved.
    #[serde(default)]
    pub require_binding: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCheckParams {
    pub checkpoint_id: String,
    pub command_id: String,
    /// Exact bytes reviewed by the caller. Never silently select a newer trusted plan.
    pub config_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStepResult {
    pub step: VerifyStep,
    /// None means this step has not been dispatched.
    pub status: Option<crate::CheckStatus>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}
