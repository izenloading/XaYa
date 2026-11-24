//! Versioned JSON-RPC types shared by XaYa clients and its agent service.
//! This crate has no process, transport, database, or async runtime dependency.

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod control;
pub use control::*;
mod landing;
pub use landing::*;
mod project;
pub use project::*;
mod review;
pub use review::*;
mod task;
pub use task::*;
mod verification;
pub use verification::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const INITIALIZE: &str = "initialize";
pub const STATUS: &str = "server.status";
pub const SHUTDOWN: &str = "server.shutdown";
pub const SESSION_START: &str = "session.start";
pub const CLAUDE_START: &str = "session.start_claude";
pub const ATTACH_CLAIM: &str = "session.attach_claim";
pub const SESSION_LIST: &str = "session.list";
pub const SESSION_GET: &str = "session.get";
pub const SESSION_ATTACH_INFO: &str = "session.attach_info";
pub const SESSION_STOP_IDLE: &str = "session.stop_idle";
pub const SESSION_STOP: &str = "session.stop";
pub const SESSION_RESUME: &str = "session.resume";
/// End a live session's tool processes (only), once no turn is running.
pub const SESSION_END_TOOLS: &str = "session.end_tools";

/// Tool processes confirmed ended; the engine and its helpers keep running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsEnded {
    pub session_id: String,
    pub ended: u32,
}
pub const BOARD_SNAPSHOT: &str = "board.snapshot";
/// Reply with the board, then push it again (`board.changed`) after any session changes.
pub const BOARD_WATCH: &str = "board.watch";
pub const BOARD_CHANGED: &str = "board.changed";
pub const NOT_FOUND: i32 = -32003;
pub const CONFLICT: i32 = -32004;
pub const NOT_READY: i32 = -32005;
pub const ENGINE_ERROR: i32 = -32006;

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const NOT_INITIALIZED: i32 = -32001;
pub const VERSION_MISMATCH: i32 = -32002;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: i64, method: &str, params: impl Serialize) -> serde_json::Result<Self> {
        Ok(Self {
            jsonrpc: "2.0".into(),
            id: RequestId::Number(id),
            method: method.into(),
            params: serde_json::to_value(params)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Success {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    pub error: RpcError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Success(Success),
    Failure(Failure),
}

impl Response {
    pub fn success(id: RequestId, result: impl Serialize) -> serde_json::Result<Self> {
        Ok(Self::Success(Success {
            jsonrpc: "2.0".into(),
            id,
            result: serde_json::to_value(result)?,
        }))
    }

    pub fn failure(id: Option<RequestId>, code: i32, message: &str) -> Self {
        Self::Failure(Failure {
            jsonrpc: "2.0".into(),
            id,
            error: RpcError { code, message: message.into() },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub protocol_version: u32,
    pub server_version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub active_sessions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownResult {
    pub accepted: bool,
}

/// This first managed-session API supports Codex in an existing working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartSessionParams {
    pub cwd: String,
    /// An absolute executable path, resolved by the invoking CLI, not the daemon's PATH.
    pub executable: String,
    /// Resolve configuration per caller rather than inheriting an older daemon's CODEX_HOME.
    pub codex_home: String,
    #[serde(default)]
    pub sandbox: Sandbox,
    /// A first message submitted without a native TUI: the service starts the main thread
    /// itself and sends this once. Without it, the first native TUI starts the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Claude's native daemon owns its worker; manual permissions do not imply an OS sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartClaudeParams {
    pub cwd: String,
    pub executable: String,
    /// The caller's CLAUDE_CONFIG_DIR, only if it set one. Claude on macOS keys its Keychain
    /// login by whether the variable is set at all, so `None` leaves it unset.
    #[serde(default)]
    pub config_dir: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sandbox {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    Manual,
}

impl Sandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Ready,
    Working,
    Blocked,
    Failed,
    Stopped,
    Interrupted,
}

impl SessionState {
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Failed | Self::Stopped | Self::Interrupted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub engine: String,
    pub cwd: String,
    pub sandbox: Sandbox,
    pub state: SessionState,
    /// Bound only by the response to a native TUI thread/start or thread/resume request.
    pub native_thread_id: Option<String>,
    #[serde(default)]
    pub native_job_id: Option<String>,
    pub turn_id: Option<String>,
    pub attached: bool,
    pub backend_pid: Option<u32>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub verification: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachInfo {
    pub session_id: String,
    pub executable: String,
    pub cwd: String,
    pub codex_home: String,
    /// Argument vector; callers must not interpret it as shell source.
    pub args: Vec<String>,
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_cannot_mix_success_and_error() {
        let invalid = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":{},
            "error":{"code":-32600,"message":"bad"}});
        assert!(serde_json::from_value::<Response>(invalid).is_err());
    }

    #[test]
    fn null_results_and_string_ids_round_trip() {
        let response =
            Response::success(RequestId::String("request-1".into()), Value::Null).unwrap();
        let wire = serde_json::to_vec(&response).unwrap();
        let Response::Success(result) = serde_json::from_slice(&wire).unwrap() else {
            panic!("expected success")
        };
        assert_eq!(result.id, RequestId::String("request-1".into()));
        assert!(result.result.is_null());
    }
}
