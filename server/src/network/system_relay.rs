//! WebSocket relay for the persistent System AI Claude --sdk-url session.
//!
//! This is Claude-specific transport infrastructure. Like `agent_relay.rs`,
//! it exists because Claude CLI connects via WebSocket. The OpenAI adapter
//! uses outbound HTTP and does not need a relay.
//!
//! NDJSON protocol handling is delegated to `crate::llm::providers::claude`
//! which owns the canonical implementation. This module only translates
//! relay events into System AI-specific channels (token tracking, GM log).

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

use crate::llm::providers::claude::{claude_format_user_message, process_ndjson_line, RelayEvent};
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
        (
            prompt_rx,
            state.token_tx.clone(),
            state.gm_log_tx.clone(),
            state.connected.clone(),
        )
    };

    tracing::info!("[system-relay] Claude WebSocket connected");
    connected.notify_waiters();

    loop {
        tokio::select! {
            Some(prompt) = prompt_rx.recv() => {
                let ndjson = claude_format_user_message(&prompt);
                if socket.send(Message::Text(ndjson.into())).await.is_err() {
                    tracing::warn!("[system-relay] WebSocket send failed");
                    break;
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        for line in text.lines() {
                            if let Err(()) = process_system_line(
                                line, &token_tx, &gm_log_tx, &mut socket,
                            ).await {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            for line in text.lines() {
                                if let Err(()) = process_system_line(
                                    line, &token_tx, &gm_log_tx, &mut socket,
                                ).await {
                                    return;
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

/// Process a single NDJSON line for the system relay.
/// Uses the shared protocol handler and translates events to System AI channels.
/// Returns Err(()) if the WebSocket connection should be closed.
async fn process_system_line(
    line: &str,
    token_tx: &mpsc::Sender<TokenUsageEvent>,
    gm_log_tx: &mpsc::Sender<GmLogEntry>,
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

            RelayEvent::Result { usage, .. } => {
                if let Some(usage) = usage {
                    let _ = token_tx
                        .send(TokenUsageEvent {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cost_usd: usage.cost_usd,
                        })
                        .await;
                }
                // System AI result text is not forwarded — verdicts come via MCP tools.
            }

            RelayEvent::AssistantText(text) => {
                // Capture as GM response log.
                let _ = gm_log_tx.try_send(GmLogEntry {
                    kind: "response",
                    text,
                });
            }

            RelayEvent::ToolUse(tool) => {
                tracing::info!("[system-relay] TOOL_USE: {}", tool);
            }

            RelayEvent::ThinkingBlock(text) => {
                // Capture GM thinking.
                let _ = gm_log_tx.try_send(GmLogEntry {
                    kind: "thinking",
                    text,
                });
            }
        }
    }

    Ok(())
}
