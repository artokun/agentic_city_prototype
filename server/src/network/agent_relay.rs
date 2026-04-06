//! Per-agent WebSocket relay for Claude CLI --sdk-url connections.
//!
//! This is Claude-specific transport infrastructure. It exists because
//! Claude CLI connects via WebSocket (`--sdk-url`). The OpenAI adapter
//! uses outbound HTTP and does not need a relay.
//!
//! NDJSON protocol handling is delegated to `crate::llm::providers::claude`
//! which owns the canonical implementation.

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

use crate::llm::providers::claude::{
    claude_format_user_message, process_ndjson_line, RelayEvent,
};

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
    /// Game -> Claude: user messages.
    prompt_rx: Option<mpsc::Receiver<String>>,
    /// Claude -> Game: extracted text responses.
    response_tx: mpsc::Sender<String>,
    /// Claude -> Game: token usage events.
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
    let (prompt_rx, response_tx, token_tx, connected) = {
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

    loop {
        tokio::select! {
            // Game -> Claude: send user messages as NDJSON.
            Some(prompt) = prompt_rx.recv() => {
                let ndjson = claude_format_user_message(&prompt);
                tracing::debug!("[relay:{}] -> Claude: user message ({}b)", agent_id, ndjson.len());
                if socket.send(Message::Text(ndjson.into())).await.is_err() {
                    tracing::warn!("[relay:{}] WebSocket send failed", agent_id);
                    break;
                }
            }

            // Claude -> Game: receive and process NDJSON messages.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        for line in text.lines() {
                            if let Err(()) = process_agent_line(
                                line, &agent_id, &response_tx, &token_tx, &mut socket,
                            ).await {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            for line in text.lines() {
                                if let Err(()) = process_agent_line(
                                    line, &agent_id, &response_tx, &token_tx, &mut socket,
                                ).await {
                                    return;
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

/// Process a single NDJSON line for an agent relay.
/// Uses the shared protocol handler and translates events to agent-specific channels.
/// Returns Err(()) if the WebSocket connection should be closed.
async fn process_agent_line(
    line: &str,
    agent_id: &str,
    response_tx: &mpsc::Sender<String>,
    token_tx: &mpsc::Sender<TokenUsageEvent>,
    socket: &mut WebSocket,
) -> Result<(), ()> {
    let events = process_ndjson_line(line);

    for event in events {
        match event {
            RelayEvent::ControlRequest { response_ndjson } => {
                if socket
                    .send(Message::Text(response_ndjson.into()))
                    .await
                    .is_err()
                {
                    return Err(());
                }
            }

            RelayEvent::Result { text, usage } => {
                if let Some(usage) = usage {
                    tracing::info!(
                        "[relay:{}] tokens: in={}, out={}, cost=${:.4}",
                        agent_id,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cost_usd,
                    );
                    let _ = token_tx
                        .send(TokenUsageEvent {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cost_usd: usage.cost_usd,
                        })
                        .await;
                }
                if let Some(text) = text {
                    let preview: String = text.chars().take(80).collect();
                    tracing::info!("[relay:{}] response: {}...", agent_id, preview);
                    let _ = response_tx.send(text).await;
                }
            }

            RelayEvent::AssistantText(text) => {
                // Forward thinking as thought-prefixed messages.
                let preview: String = text.chars().take(100).collect();
                tracing::info!("[relay:{}] THOUGHT: {}", agent_id, preview);
                let msg = format!("thought:{}", text);
                let _ = response_tx.send(msg).await;
            }

            RelayEvent::ToolUse(tool) => {
                tracing::info!("[relay:{}] TOOL_USE: {}", agent_id, tool);
            }

            RelayEvent::ThinkingBlock(_) => {
                // Agent relays don't forward thinking blocks.
            }
        }
    }

    Ok(())
}
