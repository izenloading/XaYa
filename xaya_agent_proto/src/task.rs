//! Tasks: a goal and testable acceptance criteria attached to sessions, and the handoff bundle a
//! reviewer receives for one. Criteria are fixed once created; their hashes bind reviews to them.
use serde::{Deserialize, Serialize};

pub const TASK_CREATE: &str = "task.create";
pub const TASK_RESET_LAUNCH: &str = "task.reset_launch";
pub const TASK_START: &str = "task.start";
pub const TASK_ENQUEUE: &str = "task.enqueue";
pub const TASK_DELEGATE: &str = "task.delegate";
pub const TASK_LAND: &str = "task.land";
/// What a person needs to accept a task: its change, verification, review and whether it can land.
pub const TASK_ACCEPTANCE: &str = "task.acceptance";
/// Ask for changes to a driven task's reviewed change: the request goes to its implementing
/// session, and the service verifies and reviews again.
pub const TASK_REVISE: &str = "task.revise";
/// A session message is at most 8192 bytes; the request leaves room for its framing.
pub const TASK_MAX_FEEDBACK_BYTES: usize = 6000;
pub const TASK_DEQUEUE: &str = "task.dequeue";
pub const TASK_DRIVE: &str = "task.drive";
pub const TASK_UNDRIVE: &str = "task.undrive";
pub const TASK_BOARD: &str = "task.board";
pub const TASK_CAPACITY_GET: &str = "task.capacity.get";
pub const TASK_CAPACITY_SET: &str = "task.capacity.set";
/// The engine a project's delegated tasks are implemented with unless one is named.
pub const TASK_ENGINE_GET: &str = "task.engine.get";
pub const TASK_ENGINE_SET: &str = "task.engine.set";
/// Without a project setting, Claude implements.
pub const TASK_DEFAULT_ENGINE: crate::ReviewEngine = crate::ReviewEngine::Claude;

pub const TASK_DEFAULT_CAPACITY: usize = 1;
pub const TASK_MAX_CAPACITY: usize = 16;
pub const TASK_DEPENDENCIES: &str = "task.dependencies";
pub const TASK_GET: &str = "task.get";
pub const TASK_LIST: &str = "task.list";
pub const TASK_CLOSE: &str = "task.close";
/// The open task a session belongs to, or null.
pub const TASK_FOR_SESSION: &str = "task.for_session";
/// Clear a task's need for attention and optionally grant more fix rounds.
pub const TASK_CONTINUE: &str = "task.continue";

pub const TASK_DEFAULT_FIX_BUDGET: u32 = 2;
pub const TASK_MAX_FIX_BUDGET: u32 = 5;

fn default_fix_budget() -> u32 {
    TASK_DEFAULT_FIX_BUDGET
}

pub const TASK_MAX_TITLE: usize = 120;
pub const TASK_MAX_GOAL: usize = 1200;
pub const TASK_MAX_CONSTRAINTS: usize = 800;
pub const TASK_MAX_CRITERIA: usize = 20;
pub const TASK_MAX_CRITERION: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateParams {
    /// Project checkout on creation; canonical Git common directory in stored tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Existing prerequisite tasks, fixed when this task is created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    pub title: String,
    pub goal: String,
    #[serde(default)]
    pub constraints: Option<String>,
    /// Each one testable, e.g. "cargo test passes" rather than "the code is clean".
    pub criteria: Vec<String>,
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Review rounds that may be sent back before a person decides (0-5, default 2).
    #[serde(default)]
    pub fix_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dual_review: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskContinueParams {
    pub task_id: String,
    /// Extra fix rounds to grant.
    #[serde(default)]
    pub rounds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskParams {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResetLaunchParams {
    pub task_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCloseParams {
    pub task_id: String,
    /// `accepted` or `abandoned`.
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    Accepted,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    /// `AC1`, `AC2`, … in creation order.
    pub id: String,
    pub text: String,
    /// SHA-256 of `text`.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Changes a person asked for, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<TaskRevision>,
    /// An explicit authorization to land the task's approved change, and how far it got.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<TaskLandingRecord>,
    /// The one-step request this task was created by, if any; a repeat returns this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<TaskDelegation>,
    /// An explicit authorization for the service to drive the review-and-fix cycle, and where
    /// that cycle is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<TaskDriveRecord>,
    /// An explicit background launch authorization and its durable outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<TaskQueueRecord>,
    /// Request IDs retired after confirming that their launch cannot run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_launch_requests: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<TaskLaunch>,
    /// Project checkout on creation; canonical Git common directory in stored tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Existing prerequisite tasks, fixed when this task is created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    pub id: String,
    pub title: String,
    pub goal: String,
    pub constraints: Option<String>,
    pub criteria: Vec<Criterion>,
    pub sessions: Vec<String>,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    /// Findings across the task's completed reviews.
    #[serde(default)]
    pub ledger: Vec<LedgerEntry>,
    #[serde(default = "default_fix_budget")]
    pub fix_budget: u32,
    #[serde(default)]
    pub dual_review: bool,
    /// Reviews sent back, in order.
    #[serde(default)]
    pub fix_rounds: Vec<FixRound>,
    /// Set when the loop must stop for a person; nothing is sent back until cleared.
    #[serde(default)]
    pub attention: Option<Attention>,
    /// Completed reviews in a row that were invalid.
    #[serde(default)]
    pub invalid_streak: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixRound {
    pub review_id: String,
    pub command_id: String,
    pub checkpoint_id: String,
    /// The reviewed checkpoint's tree: a later review of the same tree means nothing changed.
    pub tree: String,
    /// Ledger keys of the blocking findings that were sent.
    pub blocking: Vec<String>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub reason: String,
    pub review_id: Option<String>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispute {
    pub reason: String,
    pub review_id: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerStatus {
    Open,
    /// A later completed review no longer reported it; not proof that it was fixed.
    Resolved,
    /// Reported again after a review had not reported it.
    Reappeared,
    /// Rejected with a reason; left out of fixes until a person decides.
    Disputed,
}

/// One finding tracked across reviews, matched by path and normalized title (line numbers move
/// when code is fixed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub key: String,
    pub path: String,
    /// As last reported.
    pub title: String,
    pub status: LedgerStatus,
    pub first_review: String,
    pub last_review: String,
    /// Reviews that reported it.
    pub seen: u32,
    /// Whether it blocked when last reported.
    pub blocking: bool,
    #[serde(default)]
    pub dispute: Option<Dispute>,
}

/// What a reviewer receives about a task, generated by code, never summarized by a model. It
/// never includes earlier attempts or the implementer's conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub task_id: String,
    pub title: String,
    pub goal: String,
    pub constraints: Option<String>,
    pub criteria: Vec<Criterion>,
    /// Changed files in the reviewed range, at most 40.
    pub files: Vec<HandoffFile>,
    /// Something was cut to fit its limit.
    pub truncated: bool,
    /// SHA-256 of the bundle with this field empty.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffFile {
    pub path: String,
    /// The file's blob in the reviewed commit; null when the change deletes it.
    pub blob: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Met,
    Unmet,
    /// Cannot be confirmed by reading the code.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionResult {
    pub id: String,
    pub status: CriterionStatus,
    pub evidence: String,
}

/// Read-only evidence for one prerequisite of the queried task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencySnapshot {
    pub task_id: String,
    pub title: String,
    pub state: DependencyState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DependencyState {
    AwaitingLanding,
    MultipleLandingTargets,
    Abandoned,
    AwaitingCheckout { commit: String },
    CheckoutBehind { session_id: String, commit: String },
    CheckoutUnknown { session_id: String, commit: String },
    Included { commit: String },
}

/// Explicit local policy for task dispatch across all worktrees of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCapacityParams {
    pub cwd: String,
    /// Inclusive range: 1..=16. Lowering it never interrupts existing sessions.
    pub capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskCapacity {
    /// Canonical Git common directory shared by the project's worktrees.
    pub project: String,
    pub capacity: usize,
}

/// Set, or with no `engine` clear, a project's default implementing engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEngineParams {
    pub cwd: String,
    pub engine: Option<crate::ReviewEngine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskEngine {
    /// Canonical Git common directory shared by the project's worktrees.
    pub project: String,
    /// The setting, or the default without one.
    pub engine: crate::ReviewEngine,
    /// Whether the project sets it.
    pub configured: bool,
}

fn default_engine() -> crate::ReviewEngine {
    TASK_DEFAULT_ENGINE
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoard {
    /// The engine a new task is implemented with unless another is chosen.
    #[serde(default = "default_engine")]
    pub engine: crate::ReviewEngine,
    pub project: String,
    pub capacity: usize,
    pub occupied: usize,
    pub available: usize,
    pub tasks: Vec<TaskBoardRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoardRow {
    pub task_id: String,
    pub title: String,
    pub state: BoardState,
    pub attention: Option<Attention>,
    pub dependencies: Vec<DependencySnapshot>,
    /// The task's landing, once one was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<TaskLandingRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardState {
    Manual,
    NeedsAttention,
    /// Landing in the background.
    Landing,
    /// Its latest review is a current approval of a verified checkpoint: a person can land it.
    AwaitingAcceptance,
    ActiveSession,
    WaitingDependency,
    WaitingCapacity,
    Eligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLaunch {
    /// Set durably before entering the session launcher. Older claims lack proof of never
    /// starting.
    #[serde(default = "launch_may_have_started")]
    pub may_have_started: bool,
    pub input_hash: String,
    pub request_id: String,
    pub session_id: String,
}

fn launch_may_have_started() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStartParams {
    pub task_id: String,
    pub request_id: String,
    /// Let the service create the checkout; the engine's `cwd` is then replaced with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<TaskWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<crate::StartClaudeParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<crate::StartSessionParams>,
}

/// A checkout the service creates for one task: a detached Git worktree at the base branch's
/// tip, beside the project at `<parent>/<repository>.xaya/<task ID>`. It holds no branch;
/// checkpoints and landings do not need one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspace {
    /// Local branch to start from. Prerequisites must have landed in it.
    pub base_branch: String,
}

/// Explicitly authorize one launch in a project checkout, or in a workspace the service creates:
/// exactly one of `claude`, or `codex` with a first message (Codex then runs without a native
/// TUI until one attaches).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEnqueueParams {
    pub task_id: String,
    pub request_id: String,
    /// Let the service create the checkout; the engine's `cwd` is then replaced with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<TaskWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<crate::StartClaudeParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<crate::StartSessionParams>,
}

/// Hand one request to the service in a project checkout: it creates the task, queues its launch
/// in a workspace of its own and drives the review-and-fix cycle, all as one durable record. The
/// first line of `goal` becomes the title; the task has one criterion, that the goal is met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDelegateParams {
    pub request_id: String,
    /// A checkout of the project.
    pub cwd: String,
    pub goal: String,
    /// The workspace's base; by default the branch checked out in `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// The implementing engine. Codex needs its `config_dir` (CODEX_HOME) and may write in the
    /// workspace; the service writes the first message.
    pub implementer: crate::ReviewerSpec,
    pub reviewer: crate::ReviewerSpec,
    /// Time limit for each review and, separately, each fix turn.
    pub timeout_secs: u64,
}

/// Explicitly authorize the service to land the change the task's latest review approved:
/// preview the squash onto the target branch, verify it there, update the branch, then close the
/// task as accepted. Each step is the `land` operation of the same name; nothing is pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskLandParams {
    pub task_id: String,
    pub request_id: String,
    /// Local branch to land on; by default the base branch of the task's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLandingState {
    /// Taking steps or waiting for one to finish; `reason` says which.
    Landing,
    /// The branch holds the change and the task is accepted.
    Landed,
    /// A person has to look; `reason` says why. Land again with a new request ID.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLandingRecord {
    pub request_id: String,
    pub target_branch: String,
    pub state: TaskLandingState,
    pub reason: Option<String>,
    /// The landing preview, once created; `land show` gives its candidate and checks.
    pub preview_id: Option<String>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    /// What became of the task's workspace once the change landed; none until the service
    /// looked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<TaskWorkspaceCleanup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkspaceOutcome {
    /// Its content matched the landed checkpoint and removal began; retaken after a restart.
    Removing,
    Removed,
    /// Left in place for a person; `reason` says why.
    Kept,
    /// The task had no workspace, or it was already gone.
    Absent,
}

/// Removing a landed task's workspace: only once its content is exactly the landed checkpoint
/// (ignored files aside) and no session outside the task works in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorkspaceCleanup {
    pub outcome: TaskWorkspaceOutcome,
    pub path: Option<String>,
    pub reason: Option<String>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReviseParams {
    pub task_id: String,
    pub request_id: String,
    /// What should change, in the person's words.
    pub feedback: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRevision {
    pub request_id: String,
    pub feedback: String,
    /// The review and checkpoint the person looked at.
    pub review_id: String,
    pub checkpoint_id: String,
    pub requested_at_ms: u64,
}

/// A task's change as a person accepts it. `blocked` says why it cannot land now, if it cannot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAcceptance {
    pub task: Task,
    /// Where `task land` would put it: the landing's target, else the workspace's base branch.
    pub target_branch: Option<String>,
    /// The task's latest review and the verification it judged.
    pub review: Option<crate::Review>,
    pub check: Option<crate::Check>,
    /// Files the reviewed change touches, from its base to its checkpoint.
    pub files: Vec<ChangedFile>,
    pub diff: String,
    /// The diff was cut at its size limit; `files` is still complete.
    pub diff_truncated: bool,
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// Line counts; none for a binary file.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDelegation {
    pub request_id: String,
    /// SHA-256 of the request's parameters: the same request ID with other content is refused.
    pub input_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDequeueParams {
    pub task_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskQueueState {
    Queued,
    Waiting,
    Dispatched,
    NeedsAttention,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskQueueRecord {
    pub task_id: String,
    pub request_id: String,
    /// Let the service create the checkout; the engine's `cwd` is then replaced with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<TaskWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<crate::StartClaudeParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<crate::StartSessionParams>,
    pub state: TaskQueueState,
    pub reason: Option<String>,
    pub session_id: Option<String>,
    pub enqueued_at_ms: u64,
}

/// Explicitly authorize the service to checkpoint, verify, review and send fixes back for a task
/// until it is approved or a person has to decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDriveParams {
    pub task_id: String,
    pub request_id: String,
    /// The implementing session; by default the task's only session, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub reviewer: crate::ReviewerSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_reviewer: Option<crate::ReviewerSpec>,
    /// Time limit for each review and, separately, each fix turn.
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUndriveParams {
    pub task_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDriveState {
    /// Taking steps.
    Driving,
    /// Waiting for something outside the cycle; `reason` says what.
    Waiting,
    Approved,
    NeedsAttention,
    Stopped,
    Cancelled,
}

impl TaskDriveState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Driving | Self::Waiting)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDriveRecord {
    pub params: TaskDriveParams,
    pub state: TaskDriveState,
    pub reason: Option<String>,
    /// The implementing session, once known.
    pub session_id: Option<String>,
    /// The cycle's current phase and its rounds so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_capacity_parameters_require_a_complete_explicit_policy() {
        for value in [
            serde_json::json!({"cwd":"/project"}),
            serde_json::json!({"capacity":2}),
            serde_json::json!({"cwd":"/project","capacity":-1}),
            serde_json::json!({"cwd":"/project","capacity":1.5}),
            serde_json::json!({"cwd":"/project","capacity":2,"automatic":true}),
        ] {
            assert!(serde_json::from_value::<TaskCapacityParams>(value).is_err());
        }
        let params = TaskCapacityParams { cwd: "/project".into(), capacity: 2 };
        assert_eq!(
            serde_json::from_value::<TaskCapacityParams>(serde_json::to_value(&params).unwrap())
                .unwrap(),
            params
        );
        let policy = ProjectTaskCapacity { project: "/project/.git".into(), capacity: 2 };
        assert_eq!(
            serde_json::from_value::<ProjectTaskCapacity>(serde_json::to_value(&policy).unwrap())
                .unwrap(),
            policy
        );
    }
}
