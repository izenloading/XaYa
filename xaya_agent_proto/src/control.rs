use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Session;

pub const SESSION_SEND: &str = "session.send";
pub const SESSION_CANCEL: &str = "session.cancel";
pub const SESSION_COMMAND: &str = "session.command";
pub const APPROVAL_LIST: &str = "approval.list";
pub const APPROVAL_RESPOND: &str = "approval.respond";
pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
pub const EVENTS_EVENT: &str = "events.event";
pub const EVENTS_HEARTBEAT: &str = "events.heartbeat";
pub const EVENTS_RESYNC: &str = "events.resync_required";
pub const EVENTS_HISTORY: &str = "events.history";
pub const STALE: i32 = -32007;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlOperation {
    Send { text: String },
    Cancel { turn_id: String },
    Respond { approval_id: String, decision: ApprovalDecision },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendParams {
    pub session_id: String,
    pub command_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandParams {
    pub session_id: String,
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    pub session_id: String,
    pub command_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespondParams {
    pub session_id: String,
    pub command_id: String,
    pub approval_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Pending,
    Succeeded,
    Failed,
    Unknown,
}

/// Succeeded means the engine acknowledged a request, not that a turn or tool has finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub status: CommandStatus,
    pub turn_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Command,
    FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub kind: ApprovalKind,
    pub decisions: Vec<ApprovalDecision>,
    pub method: String,
    pub turn_id: Option<String>,
    pub details: Value,
    /// Oversized or unsupported decision formats must be answered in the native TUI.
    pub controllable: bool,
}

/// Compact panel snapshot; full approval details are fetched only for the selected session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRow {
    pub session: Session,
    pub approval_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSnapshot {
    pub session: Session,
    pub approvals: Vec<Approval>,
    pub native_only_approvals: u32,
    pub epoch: String,
    pub last_seq: u64,
}

/// The last event a client has seen. A session's stream keeps its epoch across service restarts;
/// the epoch changes only when its stored history cannot continue it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub epoch: String,
    pub seq: u64,
}

/// `after` replays every stored event following the cursor before live events; a cursor the
/// stored history cannot continue is refused with [`STALE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeParams {
    pub session_id: String,
    #[serde(default)]
    pub after: Option<EventCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    pub session_id: String,
    #[serde(default)]
    pub after_seq: u64,
}

/// One size-bounded page of the current stream's stored events after `after_seq`. Continue
/// with the last returned `seq` until it reaches `last_seq`. `first_seq` is the oldest event
/// still retained (0 when none is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventHistory {
    pub session_id: String,
    pub epoch: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub events: Vec<SessionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub session_id: String,
    pub epoch: String,
    pub seq: u64,
    #[serde(flatten)]
    pub payload: EventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    SessionChanged(Session),
    TextDelta { turn_id: String, item_id: String, text: String },
    TurnCompleted { turn_id: String, status: String },
    ApprovalRequested(Approval),
    ApprovalResolved { approval_id: String },
    NativeOnlyApprovals { count: u32 },
    CommandChanged(CommandReceipt),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}
