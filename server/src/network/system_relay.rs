//! WebSocket relay for the persistent System AI Claude --sdk-url session.
//! Unlike agent relays, assistant thinking stays internal and is never forwarded.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use bevy::prelude::Resource;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::agents::claude;
use crate::network::agent_relay::TokenUsageEvent;

#[derive(Clone, Default)]
pub struct SystemRelay {
    inner: Arc<Mutex<Option<RelayState>>>,
}

struct RelayState {
    prompt_rx: Option<mpsc::Receiver<String>>,
    token_tx: mpsc::Sender<TokenUsageEvent>,
    gm_log_tx: mpsc::Sender<GmLogEntry>,
    connected: Arc<Notify>,
}

/// A GM log entry captured from the system relay.
pub struct GmLogEntry {
    pub kind: &'static str, // "thinking" or "response"
    pub text: String,
}

pub struct RelayHandle {
    pub prompt_tx: mpsc::Sender<String>,
    pub token_rx: mpsc::Receiver<TokenUsageEvent>,
    pub gm_log_rx: mpsc::Receiver<GmLogEntry>,
    pub connected: Arc<Notify>,
}

#[derive(Resource, Clone)]
pub struct SystemRelayResource(pub SystemRelay);

impl SystemRelay {
    pub async fn register(&self) -> RelayHandle {
        let (prompt_tx, prompt_rx) = mpsc::channel::<String>(32);
        let (token_tx, token_rx) = mpsc::channel::<TokenUsageEvent>(64);
        let (gm_log_tx, gm_log_rx) = mpsc::channel::<GmLogEntry>(64);
        let connected = Arc::new(Notify::new());

        *self.inner.lock().await = Some(RelayState {
            prompt_rx: Some(prompt_rx),
            token_tx,
            gm_log_tx,
            connected: connected.clone(),
        });

        RelayHandle {
            prompt_tx,
            token_rx,
            gm_log_rx,
            connected,
        }
    }
}

pub async fn system_ws_handler(
    ws: WebSocketUpgrade,
    State(relay): State<SystemRelay>,
) -> impl IntoResponse {
    tracing::info!("[system-relay] Claude connecting");
    ws.on_upgrade(move |socket| handle_system_ws(socket, relay))
}

async fn handle_system_ws(mut socket: WebSocket, relay: SystemRelay) {
    let (mut prompt_rx, token_tx, gm_log_tx, connected) = {
        let mut state = relay.inner.lock().await;
        let Some(state) = state.as_mut() else {
            tracing::warn!("[system-relay] No relay registered");
            return;
        };
        let Some(prompt_rx) = state.prompt_rx.take() else {
            tracing::warn!("[system-relay] Already connected");
            return;
        };
        (prompt_rx, state.token_tx.clone(), state.gm_log_tx.clone(), state.connected.clone())
    };

    tracing::info!("[system-relay] Claude WebSocket connected");
    connected.notify_waiters();

    loop {
        tokio::select! {
            Some(prompt) = prompt_rx.recv() => {
                let ndjson = claude::format_user_message(&prompt);
                if socket.send(Message::Text(ndjson.into())).await.is_err() {
                    tracing::warn!("[system-relay] WebSocket send failed");
                    break;
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        for line in text.lines() {
                            if line.trim().is_empty() {
                                continue;
                            }

                            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                                continue;
                            };

                            match val.get("type").and_then(|t| t.as_str()).unwrap_or("?") {
                                "control_request" => {
                                    let request_id = val.get("request_id")
                                        .and_then(|r| r.as_str())
                                        .unwrap_or("");
                                    let tool_use_id = val.get("request")
                                        .and_then(|r| r.get("tool_use_id"))
                                        .and_then(|t| t.as_str());
                                    let input = val.get("request")
                                        .and_then(|r| r.get("input"));

                                    let response = claude::format_control_response(
                                        request_id,
                                        tool_use_id,
                                        input,
                                    );

                                    if socket.send(Message::Text(response.into())).await.is_err() {
                                        tracing::warn!("[system-relay] Failed to send control response");
                                        break;
                                    }
                                }
                                "result" => {
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
                                        let _ = token_tx.send(TokenUsageEvent {
                                            input_tokens,
                                            output_tokens,
                                            cost_usd,
                                        }).await;
                                    }
                                }
                                "assistant" => {
                                    // Capture GM thinking/responses.
                                    if let Some(msg) = val.get("message") {
                                        // Extract text content from the message.
                                        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                                            for block in content {
                                                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                                match block_type {
                                                    "thinking" => {
                                                        if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                                                            let _ = gm_log_tx.try_send(GmLogEntry {
                                                                kind: "thinking",
                                                                text: text.to_string(),
                                                            });
                                                        }
                                                    }
                                                    "text" => {
                                                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                                            let _ = gm_log_tx.try_send(GmLogEntry {
                                                                kind: "response",
                                                                text: text.to_string(),
                                                            });
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                }
                                "system" => {}
                                other => {
                                    tracing::debug!("[system-relay] {} message", other);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            for line in text.lines() {
                                if line.trim().is_empty() {
                                    continue;
                                }
                                let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                                    continue;
                                };
                                if val.get("type").and_then(|t| t.as_str()) == Some("result") {
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
                                        let _ = token_tx.send(TokenUsageEvent {
                                            input_tokens,
                                            output_tokens,
                                            cost_usd,
                                        }).await;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        tracing::info!("[system-relay] Claude disconnected");
                        break;
                    }
                }
            }
        }
    }
}
