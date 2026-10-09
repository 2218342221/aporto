//! Versioned transport-neutral contract shared by Core and HTTP adapters.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "1.0";
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const NOT_FOUND: i32 = -32004;
pub const CONFLICT: i32 = -32009;
pub const UNAVAILABLE: i32 = -32010;
pub const OVERLOADED: i32 = -32029;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}
impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}
impl std::error::Error for RpcError {}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}
impl RpcResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }
    pub fn failure(id: Option<u64>, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub client_name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_name: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    pub bundle_digest: String,
    pub release_id: String,
    pub runtime: RuntimeOptions,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOptions {
    pub provider: String,
    pub images: Vec<String>,
    pub default_image: String,
    pub default_workdir: String,
}

/// An Aporto-owned, materialized instance. Its immutable release and image are
/// shared by all attached threads; conversation history remains thread-local.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeInstance {
    pub id: String,
    pub agent_id: String,
    pub release_id: String,
    pub bundle_digest: String,
    pub provider: String,
    pub image: String,
    pub sandbox_id: String,
    pub workdir: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub busy: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstanceListParams {
    pub agent_id: String,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeInstanceListResult {
    pub instances: Vec<RuntimeInstance>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentListResult {
    pub agents: Vec<AgentSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub id: String,
    pub title: String,
    pub agent_id: String,
    pub bundle_digest: String,
    pub release_id: String,
    pub sandbox_id: Option<String>,
    pub runtime_instance_id: String,
    pub runtime_image: String,
    pub runtime_provider: String,
    pub workdir: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_sequence: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadListParams {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadStartParams {
    pub agent_id: String,
    pub title: Option<String>,
    pub runtime: Option<ThreadRuntimeParams>,
    pub workdir: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadRuntimeParams {
    New { image: Option<String> },
    Reuse { instance_id: String },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadReadParams {
    pub thread_id: String,
    pub limit: Option<u32>,
    pub before: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ThreadReadResult {
    pub thread: Thread,
    pub turns: Vec<Turn>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Interrupted,
}
impl TurnStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Turn {
    pub id: String,
    pub thread_id: String,
    pub input: String,
    pub status: TurnStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: String,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    AssistantMessage,
    PtcCall,
    ToolCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}
impl ItemStatus {
    pub fn is_terminal(self) -> bool {
        self != Self::InProgress
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemPhase {
    Commentary,
    FinalAnswer,
}

/// A bounded, cumulative public transcript snapshot, independent of model history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemContent {
    pub id: String,
    pub kind: ItemKind,
    pub status: ItemStatus,
    pub phase: Option<ItemPhase>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnItem {
    #[serde(flatten)]
    pub content: ItemContent,
    pub turn_id: String,
    /// Immutable ordering cursor: sequence of the first event for this item.
    pub ordinal: u64,
    /// Sequence of the latest persisted cumulative snapshot.
    pub sequence: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemListParams {
    pub thread_id: String,
    pub turn_id: String,
    pub after: Option<u64>,
    pub limit: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ItemListResult {
    pub items: Vec<TurnItem>,
    pub next_cursor: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub sequence: u64,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub kind: String,
    pub data: Value,
    pub created_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventListParams {
    pub thread_id: String,
    pub after: Option<u64>,
    pub limit: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EventListResult {
    pub events: Vec<Event>,
    pub next_cursor: u64,
    pub has_more: bool,
}

/// Exported schema root; all public payloads are represented by generated definitions.
#[derive(JsonSchema)]
pub struct ProtocolSchema {
    pub request: RpcRequest,
    pub response: RpcResponse,
    pub initialize: InitializeResult,
    pub agents: AgentListResult,
    pub runtime_instances: RuntimeInstanceListResult,
    pub runtime_instances_params: RuntimeInstanceListParams,
    pub thread: Thread,
    pub threads: ThreadListResult,
    pub thread_read: ThreadReadResult,
    pub turn: Turn,
    pub item: TurnItem,
    pub items: ItemListResult,
    pub events: EventListResult,
    pub initialize_params: InitializeParams,
    pub thread_start: ThreadStartParams,
    pub thread_list: ThreadListParams,
    pub thread_read_params: ThreadReadParams,
    pub turn_start: TurnStartParams,
    pub turn_interrupt: TurnInterruptParams,
    pub item_list: ItemListParams,
    pub event_list: EventListParams,
}
