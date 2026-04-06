//! Provider-neutral types for the LLM session engine.
//! No provider-specific terms (no "claude", "openai", etc.) belong here.

use serde::{Deserialize, Serialize};

/// Sentinel string sent through legacy prompt_tx channels to trigger compaction.
/// The relay loop detects this value and issues `/compact` to the Claude CLI.
pub const COMPACT_COMMAND: &str = "/compact";

/// Identifies who owns a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionOwner {
    /// A named game agent.
    Agent(String),
    /// The system AI (Game Master / bounty verifier).
    SystemAi,
    /// A one-shot research agent spawned for a topic.
    Research(String),
}

impl std::fmt::Display for SessionOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionOwner::Agent(name) => write!(f, "agent:{name}"),
            SessionOwner::SystemAi => write!(f, "system-ai"),
            SessionOwner::Research(topic) => write!(f, "research:{topic}"),
        }
    }
}

/// Commands sent into a running session.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Send a user-turn message.
    SendUserTurn(String),
    /// Return a tool result to the session.
    SendToolResult(ToolCallResult),
    /// Request context compaction.
    Compact,
    /// Shut down the session gracefully.
    Shutdown,
}

/// Events emitted by a running session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Incremental text from the model.
    TextDelta(String),
    /// The model wants to call a tool.
    ToolCallRequested(ToolCallRequest),
    /// Token usage update.
    Usage(UsageData),
    /// The model finished its turn.
    Completed,
    /// An error occurred.
    Error(String),
    /// Compaction finished successfully.
    CompactCompleted,
}

/// A tool call the model wants to make.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// Token usage data from a single turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageData {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd: f64,
}

/// Durable checkpoint for session resume / migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCheckpoint {
    pub owner: SessionOwner,
    /// Opaque provider session identifier (e.g. session ID, thread ID).
    /// `None` for providers without stable conversation IDs.
    #[serde(default)]
    pub provider_id: Option<String>,
    pub model: String,
    pub compact_threshold: u32,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub total_cost_usd: f64,
    /// Provider-specific turn marker for resumption.
    /// `None` for providers that don't use turn markers.
    #[serde(default)]
    pub last_turn_marker: Option<String>,
    /// Compacted context summary (if any).
    pub compacted_context: Option<String>,
    /// Opaque provider-specific metadata.
    /// `None` for providers with no extra state to persist.
    #[serde(default)]
    pub provider_metadata: Option<serde_json::Value>,
}

/// Error type for adapter operations.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("session not started")]
    NotStarted,
    #[error("session already running")]
    AlreadyRunning,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("channel closed")]
    ChannelClosed,
    #[error("config error: {0}")]
    Config(String),
}
