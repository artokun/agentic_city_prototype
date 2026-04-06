//! Per-agent WebSocket relay for Claude CLI --sdk-url connections.
//! Handles the full NDJSON protocol: user messages, control requests, responses.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::llm::providers::claude as claude_ndjson;

/// Token usage data extracted from Claude result messages.
#[derive(Debug, Clone)]
pub struct TokenUsageEvent {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd: f64,
}

/// Shared state for all agent relays.
#[derive(Clone, Default)]
pub struct AgentRelays {
    inner: Arc<Mutex<HashMap<String, RelayState>>>,
}

struct RelayState {
    /// Game → Claude: user messages.
    prompt_rx: Option<mpsc::Receiver<String>>,
    /// Claude → Game: extracted text responses.
    response_tx: mpsc::Sender<String>,
    /// Claude → Game: token usage events.
    token_tx: mpsc::Sender<TokenUsageEvent>,
    /// Notify when Claude connects.
    connected: Arc<Notify>,
}

/// Handle returned to the game for one agent's session.
pub struct RelayHandle {
    pub prompt_tx: mpsc::Sender<String>,
    pub response_rx: mpsc::Receiver<String>,
    pub token_rx: mpsc::Receiver<TokenUsageEvent>,
    pub connected: Arc<Notify>,
}

impl AgentRelays {
    pub async fn register(&self, agent_id: &str) -> RelayHandle {
        let (prompt_tx, prompt_rx) = mpsc::channel::<String>(32);
        let (response_tx, response_rx) = mpsc::channel::<String>(64);
        let (token_tx, token_rx) = mpsc::channel::<TokenUsageEvent>(64);
        let connected = Arc::new(Notify::new());

        self.inner.lock().await.insert(
            agent_id.to_string(),
            RelayState {
                prompt_rx: Some(prompt_rx),
                response_tx,
                token_tx,
                connected: connected.clone(),
            },
        );

        RelayHandle {
            prompt_tx,
            response_rx,
            token_rx,
            connected,
        }
    }
}

/// Axum handler: Claude CLI connects here via --sdk-url.
pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    Path(agent_id): Path<String>,
    State(relays): State<AgentRelays>,
) -> impl IntoResponse {
    tracing::info!("[relay:{}] Claude connecting", agent_id);
    ws.on_upgrade(move |socket| handle_agent_ws(socket, agent_id, relays))
}

async fn handle_agent_ws(mut socket: WebSocket, agent_id: String, relays: AgentRelays) {
    // Take channels from the relay state.
    let (mut prompt_rx, response_tx, token_tx, connected) = {
        let mut map = relays.inner.lock().await;
        let Some(state) = map.get_mut(&agent_id) else {
            tracing::warn!("[relay:{}] No relay registered", agent_id);
            return;
        };
        let rx = state.prompt_rx.take();
        (
            rx,
            state.response_tx.clone(),
            state.token_tx.clone(),
            state.connected.clone(),
        )
    };

    let Some(mut prompt_rx) = prompt_rx else {
        tracing::warn!("[relay:{}] Already connected", agent_id);
        return;
    };

    tracing::info!("[relay:{}] Claude WebSocket connected", agent_id);
    connected.notify_waiters();

    // Track if we've sent the initial prompt.
    let mut initial_sent = false;

    loop {
        tokio::select! {
            // Game → Claude: send user messages as NDJSON.
            Some(prompt) = prompt_rx.recv() => {
                let ndjson = claude_ndjson::claude_format_user_message(&prompt);
                tracing::debug!("[relay:{}] → Claude: user message ({}b)", agent_id, ndjson.len());
                if socket.send(Message::Text(ndjson.into())).await.is_err() {
                    tracing::warn!("[relay:{}] WebSocket send failed", agent_id);
                    break;
                }
                initial_sent = true;
            }

            // Claude → Game: receive NDJSON messages.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        for line in text.lines() {
                            if line.trim().is_empty() { continue; }
                            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                                tracing::debug!("[relay:{}] non-JSON: {}", agent_id, &line[..line.len().min(80)]);
                                continue;
                            };

                            let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("?");

                            // Log ALL message types and their content structure for debugging.
                            if msg_type != "system" {
                                let keys: Vec<&str> = val.as_object()
                                    .map(|o| o.keys().map(|k| k.as_str()).collect())
                                    .unwrap_or_default();
                                tracing::info!("[relay:{}] ← {} (keys: {:?})", agent_id, msg_type, keys);

                                // For assistant messages, log content block types.
                                if msg_type == "assistant" {
                                    if let Some(msg) = val.get("message") {
                                        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                                            for block in content {
                                                let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                                                if btype == "text" {
                                                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                                    let preview: String = text.chars().take(100).collect();
                                                    tracing::info!("[relay:{}] THOUGHT: {}", agent_id, preview);
                                                } else if btype == "tool_use" {
                                                    let tool = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                                    tracing::info!("[relay:{}] TOOL_USE: {}", agent_id, tool);
                                                }
                                            }
                                        }
                                    }
                                    // Also check top-level content (some formats differ).
                                    if let Some(content) = val.get("content").and_then(|c| c.as_array()) {
                                        for block in content {
                                            let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                                            if btype == "text" {
                                                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                                let preview: String = text.chars().take(100).collect();
                                                tracing::info!("[relay:{}] THOUGHT(top): {}", agent_id, preview);
                                            }
                                        }
                                    }
                                }
                            }

                            match msg_type {
                                // Auto-approve control requests.
                                "control_request" => {
                                    let request_id = val.get("request_id")
                                        .and_then(|r| r.as_str()).unwrap_or("");
                                    let subtype = val.get("request")
                                        .and_then(|r| r.get("subtype"))
                                        .and_then(|s| s.as_str()).unwrap_or("");

                                    let tool_use_id = val.get("request")
                                        .and_then(|r| r.get("tool_use_id"))
                                        .and_then(|t| t.as_str());
                                    let input = val.get("request")
                                        .and_then(|r| r.get("input"));

                                    tracing::debug!("[relay:{}] control_request: {} ({})", agent_id, subtype, request_id);

                                    let response = claude_ndjson::claude_format_control_response(
                                        request_id, tool_use_id, input,
                                    );

                                    if socket.send(Message::Text(response.into())).await.is_err() {
                                        break;
                                    }
                                }

                                // Extract text from result messages only (skip assistant to avoid duplicates).
                                "result" => {
                                    if let Some(text) = claude_ndjson::claude_extract_result_text(&val) {
                                        let preview: String = text.chars().take(80).collect();
                                        tracing::info!("[relay:{}] response: {}...", agent_id, preview);
                                        let _ = response_tx.send(text).await;
                                    }

                                    // Extract token usage from result message.
                                    let input_tokens = val.get("usage")
                                        .and_then(|u| u.get("input_tokens"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    let output_tokens = val.get("usage")
                                        .and_then(|u| u.get("output_tokens"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    let cost_usd = val.get("total_cost_usd")
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);

                                    if input_tokens > 0 || output_tokens > 0 {
                                        tracing::info!(
                                            "[relay:{}] tokens: in={}, out={}, cost=${:.4}",
                                            agent_id, input_tokens, output_tokens, cost_usd
                                        );
                                        let _ = token_tx.send(TokenUsageEvent {
                                            input_tokens,
                                            output_tokens,
                                            cost_usd,
                                        }).await;
                                    }
                                }
                                // Capture assistant thinking as dialogue.
                                "assistant" => {
                                    if let Some(text) = claude_ndjson::claude_extract_result_text(&val) {
                                        if !text.is_empty() {
                                            let msg = format!("thought:{}", text);
                                            match response_tx.send(msg.clone()).await {
                                                Ok(()) => tracing::debug!("[relay:{}] forwarded thought ({}c)", agent_id, text.len()),
                                                Err(e) => tracing::warn!("[relay:{}] thought send failed: {}", agent_id, e),
                                            }
                                        }
                                    }
                                }

                                // Skip system/hook messages silently.
                                "system" => {}

                                _ => {
                                    tracing::debug!("[relay:{}] {} message", agent_id, msg_type);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            // Re-process as text (same handler).
                            for line in text.lines() {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                                    if let Some(text) = claude_ndjson::claude_extract_result_text(&val) {
                                        let _ = response_tx.send(text).await;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong
                    _ => {
                        tracing::info!("[relay:{}] Claude disconnected", agent_id);
                        break;
                    }
                }
            }
        }
    }
}
