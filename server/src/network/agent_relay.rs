//! Per-agent WebSocket relay for Claude CLI --sdk-url connections.
//! Each agent gets a WebSocket endpoint at /agent/{id}/ws.
//! The game sends NDJSON prompts through the relay, Claude responds through it.

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    response::IntoResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};

/// Shared state for all agent relays.
#[derive(Clone, Default)]
pub struct AgentRelays {
    inner: Arc<Mutex<HashMap<String, RelayChannels>>>,
}

/// Bidirectional channels for one agent's relay.
struct RelayChannels {
    /// Game → Claude: prompts sent by the game, read by the WS handler.
    game_to_claude_tx: mpsc::Sender<String>,
    game_to_claude_rx: Option<mpsc::Receiver<String>>,
    /// Claude → Game: responses from Claude, sent by WS handler, read by game.
    claude_to_game_tx: mpsc::Sender<serde_json::Value>,
    /// Notify when Claude connects.
    connected: Arc<Notify>,
}

/// Handle returned to the game for communicating with an agent's Claude session.
pub struct RelayHandle {
    pub agent_id: String,
    pub prompt_tx: mpsc::Sender<String>,
    pub response_rx: mpsc::Receiver<serde_json::Value>,
    pub connected: Arc<Notify>,
    pub session_id: Option<String>,
}

impl AgentRelays {
    /// Register a new agent relay. Returns the handle for the game to use.
    pub async fn register(&self, agent_id: &str) -> RelayHandle {
        let (game_tx, game_rx) = mpsc::channel::<String>(32);
        let (claude_tx, claude_rx) = mpsc::channel::<serde_json::Value>(64);
        let connected = Arc::new(Notify::new());

        let channels = RelayChannels {
            game_to_claude_tx: game_tx.clone(),
            game_to_claude_rx: Some(game_rx),
            claude_to_game_tx: claude_tx,
            connected: connected.clone(),
        };

        self.inner.lock().await.insert(agent_id.to_string(), channels);

        RelayHandle {
            agent_id: agent_id.to_string(),
            prompt_tx: game_tx,
            response_rx: claude_rx,
            connected,
            session_id: None,
        }
    }
}

/// Axum handler: WebSocket endpoint for Claude CLI to connect to.
pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    Path(agent_id): Path<String>,
    State(relays): State<AgentRelays>,
) -> impl IntoResponse {
    tracing::info!("Claude connecting for agent {}", agent_id);
    ws.on_upgrade(move |socket| handle_agent_ws(socket, agent_id, relays))
}

async fn handle_agent_ws(mut socket: WebSocket, agent_id: String, relays: AgentRelays) {
    // Take the game→claude receiver from the relay.
    let (mut game_rx, claude_tx, connected) = {
        let mut map = relays.inner.lock().await;
        let Some(channels) = map.get_mut(&agent_id) else {
            tracing::warn!("No relay registered for agent {}", agent_id);
            return;
        };
        let rx = channels.game_to_claude_rx.take();
        let tx = channels.claude_to_game_tx.clone();
        let connected = channels.connected.clone();
        (rx, tx, connected)
    };

    let Some(mut game_rx) = game_rx else {
        tracing::warn!("Relay for {} already has a connection", agent_id);
        return;
    };

    tracing::info!("Claude WebSocket connected for agent {}", agent_id);
    connected.notify_waiters();

    loop {
        tokio::select! {
            // Game → Claude: forward prompts.
            Some(prompt) = game_rx.recv() => {
                if socket.send(Message::Text(prompt.into())).await.is_err() {
                    tracing::warn!("WebSocket send failed for agent {}", agent_id);
                    break;
                }
            }
            // Claude → Game: forward responses.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Parse each line as NDJSON.
                        for line in text.lines() {
                            if line.trim().is_empty() { continue; }
                            match serde_json::from_str::<serde_json::Value>(line) {
                                Ok(val) => {
                                    let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                                    tracing::debug!("[relay:{}] {} message from Claude", agent_id, msg_type);
                                    let _ = claude_tx.send(val).await;
                                }
                                Err(_) => {
                                    tracing::debug!("[relay:{}] non-JSON from Claude: {}", agent_id, &text[..text.len().min(100)]);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            for line in text.lines() {
                                if line.trim().is_empty() { continue; }
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                                    let _ = claude_tx.send(val).await;
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong
                    _ => {
                        tracing::info!("Claude WebSocket disconnected for agent {}", agent_id);
                        break;
                    }
                }
            }
        }
    }

    // Put the receiver back so Claude can reconnect.
    // (In practice, we'd need a new channel pair for reconnection.)
    tracing::info!("Claude relay closed for agent {}", agent_id);
}
