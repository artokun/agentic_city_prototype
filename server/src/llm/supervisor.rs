//! Session lifecycle supervisor — start, monitor, restart sessions.
//! Manages adapter lifecycle and integrates with durable persistence.
//! Also provides the adapter factory that role code uses to create sessions
//! without importing provider-specific modules.

use std::collections::HashMap;

use super::config::SessionProfile;
use super::persistence::CheckpointStore;
use super::session_registry::SessionHandle;
use super::types::{
    AdapterError, AgentIdentity, SessionCheckpoint, SessionCommand, SessionEvent, SessionOwner,
};
use crate::process_manager::ProcessRegistry;
use tokio::sync::mpsc;

/// Trait for provider-specific session adapters.
/// Each provider (CLI-based, API-based, etc.) implements this to translate
/// between the unified SessionCommand/SessionEvent protocol and its own wire format.
///
/// Uses async_trait for object safety with async methods.
#[async_trait::async_trait]
pub trait SessionAdapter: Send + 'static {
    /// Start a new session (or resume from checkpoint).
    async fn start(
        &mut self,
        profile: &SessionProfile,
        checkpoint: Option<&SessionCheckpoint>,
    ) -> Result<(), AdapterError>;

    /// Send a command to the running session.
    async fn send_command(&self, cmd: SessionCommand) -> Result<(), AdapterError>;

    /// Get the event receiver for this session.
    /// Called once after start() to wire into the engine.
    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<SessionEvent>>;

    /// Shut down gracefully, returning a checkpoint for later resume.
    async fn shutdown(&mut self) -> Result<Option<SessionCheckpoint>, AdapterError>;
}

/// Create paired channels for a session.
/// Returns: (handle for the registry, command_rx for the adapter, event_tx for the adapter).
/// The adapter reads commands from `command_rx` and writes events to `event_tx`.
pub fn create_handle_channels(
    profile_name: &str,
) -> (SessionHandle, mpsc::Receiver<SessionCommand>, mpsc::Sender<SessionEvent>) {
    let (command_tx, command_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let handle = SessionHandle {
        command_tx,
        event_rx,
        profile_name: profile_name.to_string(),
    };
    (handle, command_rx, event_tx)
}

// ---------------------------------------------------------------------------
// Adapter factory — role code calls these instead of importing providers.
//
// Two patterns:
//   1. `spawn_session()` — provider-neutral entry point for persistent sessions.
//      Returns a SessionBridge with prompt_tx/response_rx/token_rx channels
//      that look the same regardless of provider. Role code never matches on
//      provider_type or imports from `crate::llm::providers::*`.
//
//   2. `run_oneshot()` — one-shot session for research etc. Sends a prompt,
//      collects the full response text, returns it.
// ---------------------------------------------------------------------------

use std::sync::Arc;
use crate::llm::config::LlmConfig;
use crate::network::agent_relay::TokenUsageEvent;

/// Handle returned to role code from `spawn_session`.
/// Same shape regardless of provider — role code never knows what's underneath.
pub struct SessionBridge {
    pub prompt_tx: tokio::sync::mpsc::Sender<String>,
    pub response_rx: tokio::sync::mpsc::Receiver<String>,
    pub token_rx: tokio::sync::mpsc::Receiver<TokenUsageEvent>,
    /// Receives `true` when the provider session is ready to receive prompts.
    /// Uses `watch` instead of `Notify` so the signal is retained even if
    /// the receiver isn't registered yet (fixes OpenAI race condition).
    pub connected: tokio::sync::watch::Receiver<bool>,
    /// GM log receiver — only populated for System AI sessions using the Claude relay.
    pub gm_log_rx: Option<tokio::sync::mpsc::Receiver<crate::network::system_relay::GmLogEntry>>,
}

/// Parameters for spawning a session.
pub struct SpawnParams {
    pub profile_name: String,
    pub system_prompt: String,
    /// Agent identity (name, uuid). None for system-AI.
    pub agent_identity: Option<AgentIdentity>,
    /// WebSocket port for Claude --sdk-url. Ignored by OpenAI.
    pub ws_port: u16,
    /// Process registry for Claude PID tracking. Ignored by OpenAI.
    pub process_registry: ProcessRegistry,
    /// Agent relay for Claude WebSocket relay registration. Required for Claude agents.
    pub agent_relays: Option<crate::network::agent_relay::AgentRelays>,
    /// System relay for Claude system-AI. Required for Claude system-AI.
    pub system_relay: Option<crate::network::system_relay::SystemRelay>,
}

/// Spawn a session and return a provider-neutral bridge.
/// Reads the profile from `LlmConfig`, creates the right adapter, wires channels.
/// Role code calls this once and gets back prompt_tx/response_rx/token_rx.
pub async fn spawn_session(
    config: &LlmConfig,
    params: SpawnParams,
) -> Result<SessionBridge, String> {
    let profile = config.profile(&params.profile_name)
        .ok_or_else(|| format!("unknown profile: {}", params.profile_name))?;
    let provider = config.provider(&profile.provider)
        .ok_or_else(|| format!("unknown provider: {}", profile.provider))?;
    let model = config.effective_model(profile)
        .unwrap_or_else(|| provider.model.clone());

    match provider.provider_type.as_str() {
        "openai_responses" => spawn_openai_session(&model, profile, &params).await,
        _ => spawn_claude_session(&model, &params).await,
    }
}

/// Spawn an OpenAI session with properly wired channels.
async fn spawn_openai_session(
    model: &str,
    profile: &super::config::SessionProfile,
    params: &SpawnParams,
) -> Result<SessionBridge, String> {
    let is_system_ai = params.agent_identity.is_none() && params.system_relay.is_some();

    let mut adapter: Box<dyn SessionAdapter> = if let Some(ref identity) = params.agent_identity {
        Box::new(crate::llm::providers::openai::OpenAiAdapter::for_agent(
            &identity.name,
            &identity.uuid,
            model,
            params.system_prompt.clone(),
            profile.tool_sets.clone(),
        ))
    } else {
        Box::new(crate::llm::providers::openai::OpenAiAdapter::for_system_ai(
            model,
            params.system_prompt.clone(),
            profile.tool_sets.clone(),
        ))
    };

    adapter.start(profile, None).await.map_err(|e| e.to_string())?;

    // Take the adapter's event receiver — this is the ONLY connected one.
    let evt_rx = adapter.take_event_receiver()
        .ok_or_else(|| "adapter did not produce event receiver".to_string())?;

    // Build the bridge: prompt_tx (String) -> adapter.send_command (SessionCommand).
    // Larger buffer (256) because OpenAI processes messages slower than Claude,
    // and context updates can back up if the adapter is busy with tool calls.
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(256);
    let adapter_arc: Arc<tokio::sync::Mutex<Box<dyn SessionAdapter>>> =
        Arc::new(tokio::sync::Mutex::new(adapter));

    // Bridge: evt_rx (SessionEvent) -> response_tx (String) + token_tx.
    let (response_tx, response_rx) = mpsc::channel::<String>(64);
    let (token_tx, token_rx) = mpsc::channel::<TokenUsageEvent>(64);
    let label = if is_system_ai {
        "system-ai".to_string()
    } else {
        params.agent_identity.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| "unknown".to_string())
    };

    let adapter_for_bridge = adapter_arc.clone();
    let bridge_label = label.clone();
    tokio::spawn(async move {
        while let Some(text) = prompt_rx.recv().await {
            let cmd = if text == crate::llm::types::COMPACT_COMMAND {
                SessionCommand::Compact
            } else {
                SessionCommand::SendUserTurn(text)
            };
            let adapter = adapter_for_bridge.lock().await;
            if let Err(e) = adapter.send_command(cmd).await {
                tracing::warn!("[openai-bridge:{}] send_command failed, will retry on next message: {}", bridge_label, e);
                // Don't break — transient errors should not kill the bridge.
                // The stream_loop inside the adapter handles reconnection.
                continue;
            }
        }
    });

    tokio::spawn(async move {
        let mut evt_rx = evt_rx;
        // Accumulate text deltas into complete thoughts.
        // Flush on Completed, ToolCallRequested, Error, or when the channel closes.
        let mut pending_text = String::new();

        while let Some(event) = evt_rx.recv().await {
            match event {
                SessionEvent::TextDelta(text) => {
                    pending_text.push_str(&text);
                }
                SessionEvent::Completed | SessionEvent::CompactCompleted => {
                    // Flush accumulated text as a complete thought.
                    if !pending_text.trim().is_empty() {
                        let msg = format!("thought:{}", pending_text.trim());
                        let _ = response_tx.send(msg).await;
                        pending_text.clear();
                    }
                }
                SessionEvent::ToolCallRequested(_) => {
                    // Flush text before tool call (the thinking before the action).
                    if !pending_text.trim().is_empty() {
                        let msg = format!("thought:{}", pending_text.trim());
                        let _ = response_tx.send(msg).await;
                        pending_text.clear();
                    }
                }
                SessionEvent::Usage(usage) => {
                    let _ = token_tx
                        .send(TokenUsageEvent {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cost_usd: usage.cost_usd,
                        })
                        .await;
                }
                SessionEvent::Error(msg) => {
                    // Flush any pending text first.
                    if !pending_text.trim().is_empty() {
                        let m = format!("thought:{}", pending_text.trim());
                        let _ = response_tx.send(m).await;
                        pending_text.clear();
                    }
                    tracing::warn!("[openai:{}] session error: {}", label, msg);
                }
            }
        }
        // Flush any remaining text when channel closes.
        if !pending_text.trim().is_empty() {
            let msg = format!("thought:{}", pending_text.trim());
            let _ = response_tx.send(msg).await;
        }
    });

    // OpenAI is ready immediately (no WebSocket handshake).
    // Send `true` via watch channel — retained even if receiver not yet registered.
    let (connected_tx, connected_rx) = tokio::sync::watch::channel(true);
    drop(connected_tx); // sender not needed — value is already set

    Ok(SessionBridge {
        prompt_tx,
        response_rx,
        token_rx,
        connected: connected_rx,
        gm_log_rx: None,
    })
}

/// Spawn a Claude CLI session with relay-based channels.
async fn spawn_claude_session(
    model: &str,
    params: &SpawnParams,
) -> Result<SessionBridge, String> {
    let is_system_ai = params.agent_identity.is_none();

    if is_system_ai {
        // System AI: use system relay.
        let relay = params.system_relay.as_ref()
            .ok_or("system_relay required for claude system-ai")?;
        let handle = relay.register().await;

        crate::llm::providers::claude::spawn_system_ai_process(
            model,
            &params.system_prompt,
            params.ws_port,
            &params.process_registry,
        )
        .await?;

        // Bridge system relay handle -> SessionBridge.
        let (token_tx, token_rx) = mpsc::channel::<TokenUsageEvent>(64);
        let sys_token_rx = handle.token_rx;
        // Forward token events.
        tokio::spawn(async move {
            let mut sys_token_rx = sys_token_rx;
            while let Some(event) = sys_token_rx.recv().await {
                let _ = token_tx.send(event).await;
            }
        });

        // System relay doesn't have a response_rx in the same shape, create a dummy.
        let (_response_tx, response_rx) = mpsc::channel::<String>(64);

        // Bridge Notify -> watch<bool> for unified SessionBridge API.
        let connected_rx = notify_to_watch(handle.connected);

        Ok(SessionBridge {
            prompt_tx: handle.prompt_tx,
            response_rx,
            token_rx,
            connected: connected_rx,
            gm_log_rx: Some(handle.gm_log_rx),
        })
    } else {
        // Agent: use agent relay.
        let identity = params.agent_identity.as_ref()
            .ok_or("agent_identity required for claude agent")?;
        let relays = params.agent_relays.as_ref()
            .ok_or("agent_relays required for claude agent")?;

        let handle = relays.register(&identity.uuid).await;

        crate::llm::providers::claude::spawn_agent_process(
            identity,
            model,
            &params.system_prompt,
            params.ws_port,
            &params.process_registry,
        )
        .await?;

        let connected_rx = notify_to_watch(handle.connected);

        Ok(SessionBridge {
            prompt_tx: handle.prompt_tx,
            response_rx: handle.response_rx,
            token_rx: handle.token_rx,
            connected: connected_rx,
            gm_log_rx: None,
        })
    }
}

/// Bridge a `Notify` (from relay) into a `watch<bool>` (for SessionBridge).
/// Spawns a task that waits for the notification and sends `true`.
fn notify_to_watch(notify: Arc<tokio::sync::Notify>) -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        notify.notified().await;
        let _ = tx.send(true);
    });
    rx
}

/// Run a one-shot LLM request through the session engine.
/// Creates the right adapter from config, sends the prompt, collects the response.
/// Used for research and other short-lived tasks.
pub async fn run_oneshot(
    config: &LlmConfig,
    profile_name: &str,
    prompt: &str,
) -> Result<String, String> {
    let profile = config.profile(profile_name)
        .ok_or_else(|| format!("unknown profile: {profile_name}"))?;
    let provider = config.provider(&profile.provider)
        .ok_or_else(|| format!("unknown provider: {}", profile.provider))?;
    let model = config.effective_model(profile)
        .unwrap_or_else(|| provider.model.clone());

    match provider.provider_type.as_str() {
        "openai_responses" => run_oneshot_openai(prompt, &model).await,
        _ => run_oneshot_claude(prompt, &model).await,
    }
}

/// One-shot research via Claude CLI (`claude -p`).
async fn run_oneshot_claude(prompt: &str, model: &str) -> Result<String, String> {
    let result = tokio::process::Command::new("claude")
        .args([
            "-p", prompt,
            "--output-format", "text",
            "--model", model,
            "--permission-mode", "bypassPermissions",
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            if text.trim().is_empty() {
                Ok("Research completed but no content was produced.".to_string())
            } else {
                Ok(text)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("Process failed: {}", stderr.chars().take(200).collect::<String>()))
        }
        Err(err) => Err(format!("Failed to spawn: {err}")),
    }
}

/// One-shot research via OpenAI Responses API (non-streaming).
async fn run_oneshot_openai(prompt: &str, model: &str) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY not set".to_string())?;
    let api_base = std::env::var("OPENAI_API_BASE")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": model,
        "input": [{"role": "user", "content": prompt}],
        "stream": false,
    });

    let resp = client
        .post(format!("{api_base}/responses"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {}", &body_text[..body_text.len().min(200)]));
    }

    let val: serde_json::Value = resp.json().await
        .map_err(|e| format!("JSON parse error: {e}"))?;

    Ok(val.get("output")
        .and_then(|o| o.as_array())
        .and_then(|arr| arr.iter().find_map(|item| {
            if item.get("type")?.as_str()? == "message" {
                item.get("content")?
                    .as_array()?
                    .iter()
                    .find_map(|c| {
                        if c.get("type")?.as_str()? == "output_text" {
                            c.get("text")?.as_str().map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
            } else {
                None
            }
        }))
        .unwrap_or_else(|| "Research completed but no content was extracted.".to_string()))
}

// Legacy factory functions kept for backwards compatibility during transition.

/// Spawn a Claude CLI process for an agent session.
pub async fn spawn_agent_process(
    identity: &AgentIdentity,
    model: &str,
    system_prompt: &str,
    ws_port: u16,
    process_registry: &ProcessRegistry,
) -> Result<(), String> {
    crate::llm::providers::claude::spawn_agent_process(
        identity, model, system_prompt, ws_port, process_registry,
    ).await
}

/// Spawn a Claude CLI process for the system-AI session.
pub async fn spawn_system_ai_process(
    model: &str,
    system_prompt: &str,
    ws_port: u16,
    process_registry: &ProcessRegistry,
) -> Result<(), String> {
    crate::llm::providers::claude::spawn_system_ai_process(
        model, system_prompt, ws_port, process_registry,
    ).await
}

/// Create an OpenAI adapter for an agent session.
pub fn create_openai_agent_adapter(
    agent_name: &str,
    agent_id: &str,
    model: &str,
    system_prompt: String,
    tool_sets: Vec<String>,
) -> Box<dyn SessionAdapter> {
    Box::new(crate::llm::providers::openai::OpenAiAdapter::for_agent(
        agent_name, agent_id, model, system_prompt, tool_sets,
    ))
}

/// Create an OpenAI adapter for the system-AI session.
pub fn create_openai_system_ai_adapter(
    model: &str,
    system_prompt: String,
    tool_sets: Vec<String>,
) -> Box<dyn SessionAdapter> {
    Box::new(crate::llm::providers::openai::OpenAiAdapter::for_system_ai(
        model, system_prompt, tool_sets,
    ))
}

/// Manages adapter lifecycle, persistence, and recovery.
#[derive(bevy::prelude::Resource)]
pub struct SessionSupervisor {
    store: CheckpointStore,
    /// Cached checkpoints loaded at startup.
    checkpoints: HashMap<SessionOwner, SessionCheckpoint>,
}

impl SessionSupervisor {
    /// Create a supervisor with config-driven persistence directory.
    pub fn new() -> Self {
        let store = CheckpointStore::from_env();
        let checkpoints = store.load_all();
        if !checkpoints.is_empty() {
            tracing::info!(
                "[supervisor] loaded {} checkpoint(s) from disk",
                checkpoints.len()
            );
        }
        Self { store, checkpoints }
    }

    /// Create a supervisor with a custom store (for testing).
    pub fn with_store(store: CheckpointStore) -> Self {
        let checkpoints = store.load_all();
        Self { store, checkpoints }
    }

    /// Get a cached checkpoint for a session owner, if one exists.
    pub fn get_checkpoint(&self, owner: &SessionOwner) -> Option<&SessionCheckpoint> {
        self.checkpoints.get(owner)
    }

    /// Save a checkpoint to disk and update the cache.
    pub fn save_checkpoint(&mut self, checkpoint: SessionCheckpoint) {
        if let Err(e) = self.store.save(&checkpoint) {
            tracing::error!(
                "[supervisor] failed to save checkpoint for {}: {e}",
                checkpoint.owner
            );
            return;
        }
        self.checkpoints.insert(checkpoint.owner.clone(), checkpoint);
    }

    /// Save a checkpoint after compaction: updates token counters and clears the event log.
    pub fn save_after_compaction(
        &mut self,
        owner: &SessionOwner,
        compacted_context: Option<String>,
        total_input_tokens: u32,
        total_output_tokens: u32,
        total_cost_usd: f64,
    ) {
        if let Some(cp) = self.checkpoints.get_mut(owner) {
            cp.total_input_tokens = total_input_tokens;
            cp.total_output_tokens = total_output_tokens;
            cp.total_cost_usd = total_cost_usd;
            cp.compacted_context = compacted_context;

            if let Err(e) = self.store.save(cp) {
                tracing::error!(
                    "[supervisor] failed to save post-compaction checkpoint for {}: {e}",
                    owner
                );
                return;
            }

            // Truncate the event log since we've compacted.
            if let Err(e) = self.store.truncate_events(owner) {
                tracing::warn!(
                    "[supervisor] failed to truncate event log for {}: {e}",
                    owner
                );
            }

            tracing::info!("[supervisor] saved post-compaction checkpoint for {}", owner);
        } else {
            tracing::warn!(
                "[supervisor] no cached checkpoint for {} — cannot save after compaction",
                owner
            );
        }
    }

    /// Remove a checkpoint (e.g. when a session is permanently ended).
    pub fn remove_checkpoint(&mut self, owner: &SessionOwner) {
        self.checkpoints.remove(owner);
        if let Err(e) = self.store.remove(owner) {
            tracing::warn!(
                "[supervisor] failed to remove checkpoint for {}: {e}",
                owner
            );
        }
    }

    /// Append an event to the persistent event log for a session.
    pub fn log_event(
        &self,
        owner: &SessionOwner,
        event: &SessionEvent,
    ) {
        let persisted = super::persistence::session_event_to_persisted(event);
        if let Err(e) = self.store.append_event(owner, &persisted) {
            tracing::warn!(
                "[supervisor] failed to log event for {}: {e}",
                owner
            );
        }
    }

    /// Get a reference to the underlying checkpoint store.
    pub fn store(&self) -> &CheckpointStore {
        &self.store
    }
}
