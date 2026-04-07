//! OpenAI Responses API adapter — implements SessionAdapter for GPT-5.4.
//!
//! Owns: WebSocket streaming via `wss://api.openai.com/v1/responses`,
//! function-tool compilation, tool-result submission via auto-continue,
//! compaction via context summarization, and hybrid resume with
//! `previous_response_id` + local checkpoint.
//!
//! No OpenAI event names or wire-format details leak outside this module.

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::llm::config::SessionProfile;
use crate::llm::supervisor::SessionAdapter;
use crate::llm::tools::catalog::tools_for_set;
use crate::llm::tools::execute::{execute_game_action_async, execute_system_tool_async};
use crate::llm::tools::schema::to_openai_functions;
use crate::llm::types::{
    AdapterError, SessionCheckpoint, SessionCommand, SessionEvent, SessionOwner, ToolCallRequest,
    UsageData,
};

/// Default OpenAI API base URL (HTTP — used for deriving the WebSocket URL).
const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

/// Default WebSocket URL for the OpenAI Responses API.
const DEFAULT_WS_URL: &str = "wss://api.openai.com/v1/responses";

/// Environment variable for the API key.
const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Maximum tool-call iterations per turn to prevent infinite loops.
const MAX_TOOL_ITERATIONS: u32 = 20;

// ---------------------------------------------------------------------------
// OpenAI SSE event types (internal only — never exposed outside this module)
// ---------------------------------------------------------------------------

/// Parsed SSE event from the Responses API stream.
#[derive(Debug)]
enum SseEvent {
    /// `response.output_text.delta` — incremental text.
    TextDelta(String),
    /// `response.output_item.added` with type=function_call — captures name + IDs.
    FunctionCallStarted {
        call_id: String,
        item_id: String,
        name: String,
    },
    /// `response.function_call_arguments.done` — complete function call arguments.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// `response.completed` — response finished, includes usage.
    Completed {
        response_id: String,
        usage: Option<RawUsage>,
    },
    /// Reasoning summary from `response.output_item.done` with type=reasoning.
    ReasoningSummary(String),
    /// `response.failed` or `response.incomplete`.
    Error(String),
    /// Events we don't need to act on.
    Ignored,
}

/// Raw usage data as returned by OpenAI.
#[derive(Debug, Clone)]
struct RawUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ---------------------------------------------------------------------------
// SSE parsing (event payloads are identical for HTTP SSE and WebSocket)
// ---------------------------------------------------------------------------

/// Parse a single JSON event payload from the Responses API.
/// Works for both HTTP SSE (after stripping `data: ` prefix) and WebSocket
/// (where each message is a complete JSON object).
fn parse_sse_data(json_str: &str) -> SseEvent {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return SseEvent::Ignored;
    };

    let event_type = val
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match event_type {
        "response.output_text.delta" => {
            let delta = val
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            if delta.is_empty() {
                SseEvent::Ignored
            } else {
                SseEvent::TextDelta(delta)
            }
        }

        "response.function_call_arguments.done" => {
            let call_id = val
                .get("call_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let item_id = val
                .get("item_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let effective_id = if !call_id.is_empty() { call_id } else { item_id };
            let name = val
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = val
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}")
                .to_string();
            SseEvent::FunctionCall {
                call_id: effective_id,
                name,
                arguments,
            }
        }

        "response.completed" => {
            let response = val.get("response");
            let response_id = response
                .and_then(|r| r.get("id"))
                .and_then(|id| id.as_str())
                .unwrap_or("")
                .to_string();
            let usage = response
                .and_then(|r| r.get("usage"))
                .and_then(|u| {
                    let input = u
                        .get("input_tokens")
                        .or_else(|| u.get("prompt_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let output = u
                        .get("output_tokens")
                        .or_else(|| u.get("completion_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    Some(RawUsage {
                        input_tokens: input,
                        output_tokens: output,
                    })
                });
            SseEvent::Completed { response_id, usage }
        }

        "response.failed" | "response.incomplete" => {
            let reason = val
                .get("response")
                .and_then(|r| r.get("status_details"))
                .and_then(|s| s.get("reason"))
                .and_then(|r| r.as_str())
                .unwrap_or("unknown error");
            SseEvent::Error(format!("{event_type}: {reason}"))
        }

        // Function call item added — captures the function name + call_id.
        "response.output_item.added" => {
            let item = val.get("item").unwrap_or(&val);
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if item_type == "function_call" {
                let item_id = item.get("id")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let call_id = item.get("call_id")
                    .and_then(|c| c.as_str())
                    .unwrap_or(&item_id)
                    .to_string();
                let name = item.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                SseEvent::FunctionCallStarted { call_id, item_id, name }
            } else {
                SseEvent::Ignored
            }
        }

        // Reasoning summary arrives in output_item.done with type=reasoning.
        "response.output_item.done" => {
            let item = val.get("item").unwrap_or(&val);
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if item_type == "reasoning" {
                // Extract summary text from the summary array.
                let summary_text = item
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|entry| {
                                if entry.get("type").and_then(|t| t.as_str()) == Some("summary_text") {
                                    entry.get("text").and_then(|t| t.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !summary_text.is_empty() {
                    SseEvent::ReasoningSummary(summary_text)
                } else {
                    SseEvent::Ignored
                }
            } else {
                SseEvent::Ignored
            }
        }

        // Output item events we can ignore.
        "response.created"
        | "response.in_progress"
        | "response.content_part.added"
        | "response.content_part.done"
        | "response.output_text.done"
        | "response.function_call_arguments.delta" => SseEvent::Ignored,

        _ => {
            tracing::debug!("[openai] ignoring event: {event_type}");
            SseEvent::Ignored
        }
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// Build the JSON body for a `response.create` event sent over WebSocket.
fn build_request_body(
    model: &str,
    input: &serde_json::Value,
    tools: &[serde_json::Value],
    system_prompt: Option<&str>,
    previous_response_id: Option<&str>,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "type": "response.create",
        "model": model,
        "input": input,
        "stream": true,
        "store": false,
    });

    if !tools.is_empty() {
        body["tools"] = serde_json::json!(tools);
    }

    if let Some(prompt) = system_prompt {
        body["instructions"] = serde_json::json!(prompt);
    }

    if let Some(prev_id) = previous_response_id {
        body["previous_response_id"] = serde_json::json!(prev_id);
    }

    if let Some(effort) = reasoning_effort {
        body["reasoning"] = serde_json::json!({ "effort": effort, "summary": "auto" });
    }

    body
}

/// Build input for a user message.
fn user_message_input(text: &str) -> serde_json::Value {
    serde_json::json!([{
        "role": "user",
        "content": text
    }])
}

/// Build input for a tool result submission.
fn tool_result_input(call_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!([{
        "type": "function_call_output",
        "call_id": call_id,
        "output": output
    }])
}

/// Build input for compaction — a system-level summarization request.
fn compaction_input(context_summary: &str) -> serde_json::Value {
    serde_json::json!([{
        "role": "user",
        "content": format!(
            "Previous context has been compacted. Here is the summary of what happened so far:\n\n{}",
            context_summary
        )
    }])
}

// ---------------------------------------------------------------------------
// WebSocket connection management
// ---------------------------------------------------------------------------

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Establish a WebSocket connection to the OpenAI Responses API.
async fn ws_connect(ws_url: &str, api_key: &str) -> Result<WsStream, String> {
    use tokio_tungstenite::tungstenite::http::Request;

    // Ensure rustls crypto provider is installed (required for wss://).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let request = Request::builder()
        .uri(ws_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Host", host_from_url(ws_url))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|e| format!("Failed to build WS request: {e}"))?;

    let (ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?;

    Ok(ws)
}

/// Extract the host from a URL string for the Host header.
fn host_from_url(url: &str) -> String {
    url.replace("wss://", "")
        .replace("ws://", "")
        .split('/')
        .next()
        .unwrap_or("api.openai.com")
        .to_string()
}

/// Derive the WebSocket URL from the configured API base.
/// If `OPENAI_API_BASE` starts with `wss://`, use it directly.
/// Otherwise derive from the HTTP base URL.
fn derive_ws_url(api_base: &str) -> String {
    if api_base.starts_with("wss://") || api_base.starts_with("ws://") {
        // Already a WebSocket URL — ensure it ends with /responses.
        if api_base.ends_with("/responses") {
            api_base.to_string()
        } else {
            format!("{}/responses", api_base.trim_end_matches('/'))
        }
    } else {
        // Convert HTTP base to WSS.
        let base = api_base
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        let base = base.trim_end_matches('/');
        format!("{base}/responses")
    }
}

// ---------------------------------------------------------------------------
// Stream processing types
// ---------------------------------------------------------------------------

/// A tool call collected from the response stream.
struct CollectedToolCall {
    call_id: String,
    name: String,
    arguments: serde_json::Value,
}

/// Configuration for the streaming task.
#[derive(Clone)]
struct StreamConfig {
    api_base: String,
    api_key: String,
    model: String,
    system_prompt: String,
    tool_sets: Vec<String>,
    label: String,
    /// Agent identity for tool execution (name, id). None for system-AI.
    agent_identity: Option<(String, String)>,
    /// Reasoning effort level for GPT-5.4 (e.g. "low", "medium", "high").
    reasoning_effort: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket event processing
// ---------------------------------------------------------------------------

/// Send a JSON message over the WebSocket.
async fn ws_send(ws: &mut WsStream, msg: &serde_json::Value) -> Result<(), String> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let text = serde_json::to_string(msg).map_err(|e| format!("JSON serialize error: {e}"))?;
    ws.send(Message::Text(text.into()))
        .await
        .map_err(|e| format!("WebSocket send error: {e}"))
}

/// Process WebSocket messages until a `response.completed` event arrives.
/// Returns the list of tool calls (empty if the model produced only text).
/// Emits text deltas, usage, and errors via `event_tx`.
async fn process_events_until_done(
    ws: &mut WsStream,
    event_tx: &mpsc::Sender<SessionEvent>,
    config: &StreamConfig,
) -> Result<(String, Vec<CollectedToolCall>), String> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut response_id = String::new();
    let mut tool_calls = Vec::new();

    // Track function names from output_item.added events.
    let mut pending_function_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Map item_id -> call_id.
    let mut item_to_call_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    loop {
        // Timeout after 120 seconds of no WebSocket messages — reconnect on next turn.
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ws.next(),
        )
            .await
            .map_err(|_| format!("[{}] WebSocket read timeout (120s)", config.label))?
            .ok_or_else(|| "WebSocket closed unexpectedly".to_string())?
            .map_err(|e| format!("WebSocket read error: {e}"))?;

        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(_) => continue,
            Message::Pong(_) => continue,
            Message::Close(_) => return Err("WebSocket closed by server".to_string()),
            Message::Binary(_) | Message::Frame(_) => continue,
        };

        match parse_sse_data(&text) {
            SseEvent::TextDelta(delta) => {
                let _ = event_tx.send(SessionEvent::TextDelta(delta)).await;
            }
            SseEvent::ReasoningSummary(summary) => {
                // Reasoning summaries are the model's "thinking" — emit as text.
                tracing::info!("[{}] REASONING: {}...", config.label, &summary[..summary.len().min(80)]);
                let _ = event_tx.send(SessionEvent::TextDelta(summary)).await;
            }
            SseEvent::FunctionCallStarted { call_id, item_id, name } => {
                tracing::info!(
                    "[{}] FN_START: name={} call_id={} item_id={}",
                    config.label, name, call_id, item_id
                );
                if !name.is_empty() {
                    pending_function_names.insert(call_id.clone(), name.clone());
                    if !item_id.is_empty() && item_id != call_id {
                        pending_function_names.insert(item_id.clone(), name);
                        item_to_call_id.insert(item_id, call_id);
                    }
                }
            }
            SseEvent::FunctionCall {
                call_id: raw_id,
                name,
                arguments,
            } => {
                let call_id = item_to_call_id.remove(&raw_id).unwrap_or(raw_id.clone());
                let resolved_name = if name.is_empty() {
                    pending_function_names.remove(&call_id)
                        .or_else(|| pending_function_names.remove(&raw_id))
                        .unwrap_or_default()
                } else {
                    pending_function_names.remove(&call_id);
                    pending_function_names.remove(&raw_id);
                    name
                };
                let args: serde_json::Value =
                    serde_json::from_str(&arguments).unwrap_or(serde_json::json!({}));
                tracing::info!("[{}] TOOL_CALL: {} ({})", config.label, resolved_name, call_id);

                // Emit ToolCallRequested so the bridge flushes accumulated text as a thought.
                let _ = event_tx.send(SessionEvent::ToolCallRequested(ToolCallRequest {
                    id: call_id.clone(),
                    name: resolved_name.clone(),
                    arguments: args.clone(),
                })).await;

                tool_calls.push(CollectedToolCall {
                    call_id,
                    name: resolved_name,
                    arguments: args,
                });
            }
            SseEvent::Completed { response_id: rid, usage } => {
                response_id = rid;
                if let Some(u) = usage {
                    tracing::info!(
                        "[{}] usage: in={}, out={}",
                        config.label,
                        u.input_tokens,
                        u.output_tokens
                    );
                    let cost_usd = estimate_cost(u.input_tokens, u.output_tokens);
                    let _ = event_tx
                        .send(SessionEvent::Usage(UsageData {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cost_usd,
                        }))
                        .await;
                }
                // response.completed — this response is done.
                break;
            }
            SseEvent::Error(msg) => {
                tracing::warn!("[{}] stream error: {}", config.label, msg);
                let _ = event_tx.send(SessionEvent::Error(msg.clone())).await;
                return Err(msg);
            }
            SseEvent::Ignored => {}
        }
    }

    Ok((response_id, tool_calls))
}

// ---------------------------------------------------------------------------
// Stream loop (persistent WebSocket event loop)
// ---------------------------------------------------------------------------

/// Run the streaming loop: reads commands, sends `response.create` events over
/// a persistent WebSocket, executes tool calls through the shared local tool
/// runtime, and emits canonical SessionEvent values.
///
/// The WebSocket is connected lazily on the first `SendUserTurn` and reconnected
/// automatically if it drops (60 minute server-side timeout).
async fn stream_loop(
    mut command_rx: mpsc::Receiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
    config: StreamConfig,
    initial_previous_response_id: Option<String>,
    shared_response_id: Arc<tokio::sync::Mutex<Option<String>>>,
) {
    let tools = compile_tools(&config.tool_sets);
    let ws_url = derive_ws_url(&config.api_base);
    let mut previous_response_id = initial_previous_response_id;
    let mut ws: Option<WsStream> = None;

    // Session-scoped document inspection tracking for System AI policy enforcement.
    let mut inspected_docs = std::collections::HashSet::new();

    while let Some(cmd) = command_rx.recv().await {
        // Drain any additional queued commands into a batch.
        // This prevents context updates from piling up while the adapter
        // was busy with a long API response.
        let mut cmds = vec![cmd];
        while let Ok(extra) = command_rx.try_recv() {
            cmds.push(extra);
        }
        // Process only the LAST SendUserTurn (most recent context), skip stale ones.
        let cmd = if cmds.len() > 1 {
            let mut last_turn = None;
            let mut other = None;
            for c in cmds.into_iter().rev() {
                match c {
                    SessionCommand::SendUserTurn(_) if last_turn.is_none() => last_turn = Some(c),
                    SessionCommand::SendUserTurn(_) => {
                        tracing::debug!("[{}] dropping stale context update", config.label);
                    }
                    SessionCommand::Shutdown => { other = Some(c); break; }
                    SessionCommand::Compact => { other = Some(c); }
                    _ => {}
                }
            }
            other.or(last_turn).unwrap_or(SessionCommand::SendUserTurn(String::new()))
        } else {
            cmds.into_iter().next().unwrap()
        };

        match cmd {
            SessionCommand::SendUserTurn(text) if text.is_empty() => continue,
            SessionCommand::SendUserTurn(text) => {
                // Ensure we have a live WebSocket connection (lazy connect / reconnect).
                if ws.is_none() {
                    tracing::info!("[{}] connecting to {}", config.label, ws_url);
                    match ws_connect(&ws_url, &config.api_key).await {
                        Ok(stream) => ws = Some(stream),
                        Err(e) => {
                            tracing::error!("[{}] WS connect failed: {e}", config.label);
                            let _ = event_tx.send(SessionEvent::Error(e)).await;
                            continue;
                        }
                    }
                }

                let input = user_message_input(&text);

                match run_ws_tool_loop(
                    ws.as_mut().unwrap(),
                    &config,
                    &tools,
                    &input,
                    Some(&config.system_prompt),
                    previous_response_id.as_deref(),
                    &event_tx,
                    &mut inspected_docs,
                )
                .await
                {
                    Ok(resp_id) => {
                        if !resp_id.is_empty() {
                            previous_response_id = Some(resp_id.clone());
                            *shared_response_id.lock().await = Some(resp_id);
                        }
                        // Turn completed (no more tool calls).
                        let _ = event_tx.send(SessionEvent::Completed).await;
                    }
                    Err(e) => {
                        tracing::error!("[{}] stream error: {e}", config.label);
                        let _ = event_tx.send(SessionEvent::Error(e)).await;
                        // Drop the broken connection so we reconnect on next turn.
                        ws = None;
                        previous_response_id = None;
                        *shared_response_id.lock().await = None;
                    }
                }
            }

            SessionCommand::SendToolResult(_) => {
                // Tool results are handled internally by run_ws_tool_loop.
                tracing::warn!(
                    "[{}] ignoring external SendToolResult — tools are executed internally",
                    config.label
                );
            }

            SessionCommand::Compact => {
                tracing::info!("[{}] compacting — resetting response chain", config.label);

                // Clear previous_response_id to break the chain.
                previous_response_id = None;
                *shared_response_id.lock().await = None;

                // Ensure we have a live connection.
                if ws.is_none() {
                    match ws_connect(&ws_url, &config.api_key).await {
                        Ok(stream) => ws = Some(stream),
                        Err(e) => {
                            tracing::error!("[{}] WS connect failed during compact: {e}", config.label);
                            let _ = event_tx.send(SessionEvent::Error(e)).await;
                            continue;
                        }
                    }
                }

                let input = compaction_input(
                    "Session compacted. Previous response chain cleared.",
                );
                let body = build_request_body(
                    &config.model,
                    &input,
                    &tools,
                    Some(&config.system_prompt),
                    None, // deliberately break the chain
                    config.reasoning_effort.as_deref(),
                );

                let ws_ref = ws.as_mut().unwrap();
                match ws_send(ws_ref, &body).await {
                    Ok(()) => {
                        match process_events_until_done(ws_ref, &event_tx, &config).await {
                            Ok((resp_id, _)) => {
                                if !resp_id.is_empty() {
                                    previous_response_id = Some(resp_id.clone());
                                    *shared_response_id.lock().await = Some(resp_id);
                                }
                                let _ = event_tx.send(SessionEvent::CompactCompleted).await;
                            }
                            Err(e) => {
                                tracing::error!("[{}] compaction error: {e}", config.label);
                                let _ = event_tx.send(SessionEvent::Error(e)).await;
                                ws = None;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("[{}] compaction send error: {e}", config.label);
                        let _ = event_tx.send(SessionEvent::Error(e)).await;
                        ws = None;
                    }
                }
            }

            SessionCommand::Shutdown => {
                tracing::info!("[{}] shutdown requested", config.label);
                // Close the WebSocket gracefully.
                if let Some(ref mut stream) = ws {
                    use futures_util::SinkExt;
                    use tokio_tungstenite::tungstenite::Message;
                    let _ = stream.send(Message::Close(None)).await;
                }
                break;
            }
        }
    }

    tracing::info!("[{}] stream loop exited", config.label);
}

/// Send a `response.create` and loop on tool calls over the persistent WebSocket
/// until the model produces a final response (no tool calls).
async fn run_ws_tool_loop(
    ws: &mut WsStream,
    config: &StreamConfig,
    tools: &[serde_json::Value],
    initial_input: &serde_json::Value,
    system_prompt: Option<&str>,
    initial_prev_id: Option<&str>,
    event_tx: &mpsc::Sender<SessionEvent>,
    inspected_docs: &mut std::collections::HashSet<String>,
) -> Result<String, String> {
    let mut current_input = initial_input.clone();
    let mut prev_id = initial_prev_id.map(|s| s.to_string());
    let mut iteration = 0;

    loop {
        iteration += 1;
        if iteration > MAX_TOOL_ITERATIONS {
            tracing::warn!(
                "[{}] hit max tool iterations ({})",
                config.label,
                MAX_TOOL_ITERATIONS
            );
            break;
        }

        // Build and send response.create over the WebSocket.
        let body = build_request_body(
            &config.model,
            &current_input,
            tools,
            if iteration == 1 { system_prompt } else { None },
            prev_id.as_deref(),
            config.reasoning_effort.as_deref(),
        );

        tracing::debug!(
            "[{}] response.create (model={}, iteration={})",
            config.label,
            config.model,
            iteration
        );

        ws_send(ws, &body).await?;

        // Process events until response.completed.
        let (response_id, tool_calls) =
            process_events_until_done(ws, event_tx, config).await?;

        if !response_id.is_empty() {
            prev_id = Some(response_id.clone());
        }

        if tool_calls.is_empty() {
            // No tool calls — model produced a final response. Turn is done.
            return Ok(response_id);
        }

        // Execute tool calls through the shared local tool runtime.
        let mut tool_outputs = Vec::new();
        for tc in &tool_calls {
            // Emit ToolCallRequested so the supervisor can log it.
            let _ = event_tx
                .send(SessionEvent::ToolCallRequested(ToolCallRequest {
                    id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                }))
                .await;

            let tool_result = execute_tool_call(
                &tc.name,
                &tc.arguments,
                config.agent_identity.as_ref(),
                Some(inspected_docs),
            )
            .await;

            tracing::info!(
                "[{}] tool {} -> {} (err={})",
                config.label,
                tc.name,
                &tool_result.output[..tool_result.output.len().min(80)],
                tool_result.is_error,
            );

            tool_outputs.push(tool_result_input(&tc.call_id, &tool_result.output));
        }

        // Build combined tool results as input for the next response.create.
        let mut combined_results = Vec::new();
        for output in &tool_outputs {
            if let Some(arr) = output.as_array() {
                combined_results.extend(arr.iter().cloned());
            }
        }
        current_input = serde_json::Value::Array(combined_results);
    }

    Ok(prev_id.unwrap_or_default())
}

/// Execute a single tool call through the shared local tool runtime.
async fn execute_tool_call(
    tool_name: &str,
    arguments: &serde_json::Value,
    agent_identity: Option<&(String, String)>,
    inspected_docs: Option<&mut std::collections::HashSet<String>>,
) -> crate::llm::types::ToolCallResult {
    match tool_name {
        "game_action" => {
            let (name, id) = agent_identity
                .map(|(n, i)| (n.as_str(), i.as_str()))
                .unwrap_or(("unknown", ""));
            execute_game_action_async(arguments, name, id).await
        }
        _ => {
            // System tools — pass inspection tracking for policy enforcement.
            execute_system_tool_async(tool_name, arguments, inspected_docs).await
        }
    }
}

/// Compile tools from the tool sets specified in the profile.
fn compile_tools(tool_sets: &[String]) -> Vec<serde_json::Value> {
    let mut all_tools = Vec::new();
    for set_name in tool_sets {
        let defs = tools_for_set(set_name);
        all_tools.extend(to_openai_functions(&defs));
    }
    all_tools
}

/// Estimate USD cost from token counts.
/// Placeholder rates — update when GPT-5.4 pricing is published.
fn estimate_cost(input_tokens: u32, output_tokens: u32) -> f64 {
    // GPT-5.4 pricing placeholder: $2.50/1M input, $10.00/1M output.
    let input_cost = (input_tokens as f64) * 2.50 / 1_000_000.0;
    let output_cost = (output_tokens as f64) * 10.00 / 1_000_000.0;
    input_cost + output_cost
}

// ---------------------------------------------------------------------------
// OpenAiAdapter
// ---------------------------------------------------------------------------

/// OpenAI Responses API session adapter.
///
/// Streams responses via a persistent WebSocket connection to the Responses API,
/// compiles shared tool catalog into OpenAI function tools, and implements
/// hybrid resume with local checkpoint + `previous_response_id`.
pub struct OpenAiAdapter {
    /// Model to use (e.g. "gpt-5.4").
    model: String,
    /// API base URL (HTTP — used to derive WebSocket URL).
    api_base: String,
    /// Command sender — used by send_command().
    command_tx: Option<mpsc::Sender<SessionCommand>>,
    /// Command receiver — created in constructor, consumed by start().
    command_rx: Option<mpsc::Receiver<SessionCommand>>,
    /// Event receiver — created in constructor, taken once by take_event_receiver().
    event_rx: Option<mpsc::Receiver<SessionEvent>>,
    /// Event sender — created in constructor, consumed by start() for the stream loop.
    event_tx: Option<mpsc::Sender<SessionEvent>>,
    /// System prompt content.
    system_prompt: String,
    /// Tool set names (e.g. ["game"] or ["system"]).
    tool_sets: Vec<String>,
    /// Label for logging.
    label: String,
    /// Handle to the background streaming task.
    stream_task: Option<tokio::task::JoinHandle<()>>,
    /// Shared response ID — updated by stream_loop, read at shutdown.
    shared_response_id: Arc<tokio::sync::Mutex<Option<String>>>,
    /// API key (resolved at start time).
    api_key_env: String,
    /// Agent identity for tool execution (name, uuid). None for system-AI.
    agent_identity: Option<(String, String)>,
    /// Session owner for checkpoint metadata.
    session_owner: SessionOwner,
    /// Reasoning effort level (e.g. "medium"). None to omit.
    reasoning_effort: Option<String>,
}

impl OpenAiAdapter {
    /// Create a new adapter for an agent session.
    pub fn for_agent(
        agent_name: &str,
        agent_id: &str,
        model: &str,
        system_prompt: String,
        tool_sets: Vec<String>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<SessionEvent>(256);
        Self {
            model: model.to_string(),
            api_base: std::env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
            command_tx: Some(cmd_tx),
            command_rx: Some(cmd_rx),
            event_rx: Some(evt_rx),
            event_tx: Some(evt_tx),
            system_prompt,
            tool_sets,
            label: format!("openai:{agent_name}"),
            stream_task: None,
            shared_response_id: Arc::new(tokio::sync::Mutex::new(None)),
            api_key_env: API_KEY_ENV.to_string(),
            agent_identity: Some((agent_name.to_string(), agent_id.to_string())),
            session_owner: SessionOwner::Agent(agent_name.to_string()),
            reasoning_effort: Some("low".to_string()),
        }
    }

    /// Create a new adapter for the system-AI session.
    pub fn for_system_ai(
        model: &str,
        system_prompt: String,
        tool_sets: Vec<String>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<SessionEvent>(256);
        Self {
            model: model.to_string(),
            api_base: std::env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
            command_tx: Some(cmd_tx),
            command_rx: Some(cmd_rx),
            event_rx: Some(evt_rx),
            event_tx: Some(evt_tx),
            system_prompt,
            tool_sets,
            label: "openai:system-ai".to_string(),
            stream_task: None,
            shared_response_id: Arc::new(tokio::sync::Mutex::new(None)),
            api_key_env: API_KEY_ENV.to_string(),
            agent_identity: None,
            session_owner: SessionOwner::SystemAi,
            reasoning_effort: Some("low".to_string()),
        }
    }

    /// Resolve the API key from environment.
    fn resolve_api_key(&self) -> Result<String, AdapterError> {
        std::env::var(&self.api_key_env).map_err(|_| {
            AdapterError::Config(format!(
                "Missing {} environment variable",
                self.api_key_env
            ))
        })
    }
}

#[async_trait::async_trait]
impl SessionAdapter for OpenAiAdapter {
    async fn start(
        &mut self,
        profile: &SessionProfile,
        checkpoint: Option<&SessionCheckpoint>,
    ) -> Result<(), AdapterError> {
        if self.command_rx.is_none() {
            return Err(AdapterError::AlreadyRunning);
        }

        // Override model from profile if set.
        if let Some(ref m) = profile.model {
            self.model = m.clone();
        }

        // Override tool sets from profile if set.
        if !profile.tool_sets.is_empty() {
            self.tool_sets = profile.tool_sets.clone();
        }

        let api_key = self.resolve_api_key()?;

        // Hybrid resume: restore previous_response_id from checkpoint metadata.
        let previous_response_id = checkpoint
            .and_then(|cp| {
                cp.provider_metadata
                    .as_ref()
                    .and_then(|m| m.get("previous_response_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                // Try to get from shared state (sync check).
                self.shared_response_id.try_lock().ok().and_then(|g| g.clone())
            });

        // Take pre-created channels (created in constructor).
        let cmd_rx = self.command_rx.take()
            .ok_or_else(|| AdapterError::Config("command_rx already consumed".into()))?;
        let evt_tx = self.event_tx.take()
            .ok_or_else(|| AdapterError::Config("event_tx already consumed".into()))?;

        let config = StreamConfig {
            api_base: self.api_base.clone(),
            api_key,
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            tool_sets: self.tool_sets.clone(),
            label: self.label.clone(),
            agent_identity: self.agent_identity.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        };

        // Spawn the streaming task with shared response ID for shutdown access.
        let shared_rid = self.shared_response_id.clone();
        let handle = tokio::spawn(stream_loop(cmd_rx, evt_tx, config, previous_response_id, shared_rid));
        self.stream_task = Some(handle);

        tracing::info!(
            "[{}] started (model={}, tools={:?})",
            self.label,
            self.model,
            self.tool_sets
        );

        Ok(())
    }

    async fn send_command(&self, cmd: SessionCommand) -> Result<(), AdapterError> {
        let tx = self.command_tx.as_ref().ok_or(AdapterError::NotStarted)?;
        tx.send(cmd)
            .await
            .map_err(|_| AdapterError::ChannelClosed)
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<SessionEvent>> {
        self.event_rx.take()
    }

    async fn shutdown(&mut self) -> Result<Option<SessionCheckpoint>, AdapterError> {
        // Send shutdown command if channel is still open.
        if let Some(ref tx) = self.command_tx {
            let _ = tx.send(SessionCommand::Shutdown).await;
        }

        // Wait for the streaming task to finish.
        if let Some(handle) = self.stream_task.take() {
            let _ = handle.await;
        }

        self.command_tx = None;
        self.event_rx = None;

        // Read the last response ID from shared state (updated by stream_loop).
        let last_response_id = self.shared_response_id.lock().await.clone();

        let metadata = last_response_id.as_ref().map(|rid| {
            serde_json::json!({ "previous_response_id": rid })
        });

        Ok(Some(SessionCheckpoint {
            owner: self.session_owner.clone(),
            provider_id: last_response_id,
            model: self.model.clone(),
            compact_threshold: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            last_turn_marker: None,
            compacted_context: None,
            provider_metadata: metadata,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta_event() {
        let data = r#"{"type":"response.output_text.delta","delta":"Hello"}"#;
        match parse_sse_data(data) {
            SseEvent::TextDelta(t) => assert_eq!(t, "Hello"),
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn parse_function_call_done_event() {
        let data = r#"{
            "type": "response.function_call_arguments.done",
            "call_id": "call_abc123",
            "name": "game_action",
            "arguments": "{\"action\":\"look_around\"}"
        }"#;
        match parse_sse_data(data) {
            SseEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "call_abc123");
                assert_eq!(name, "game_action");
                assert!(arguments.contains("look_around"));
            }
            other => panic!("expected FunctionCall, got {:?}", other),
        }
    }

    #[test]
    fn parse_completed_event_with_usage() {
        let data = r#"{
            "type": "response.completed",
            "response": {
                "id": "resp_xyz789",
                "usage": {
                    "input_tokens": 500,
                    "output_tokens": 100
                }
            }
        }"#;
        match parse_sse_data(data) {
            SseEvent::Completed { response_id, usage } => {
                assert_eq!(response_id, "resp_xyz789");
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 500);
                assert_eq!(u.output_tokens, 100);
            }
            other => panic!("expected Completed, got {:?}", other),
        }
    }

    #[test]
    fn parse_error_event() {
        let data = r#"{
            "type": "response.failed",
            "response": {
                "status_details": {
                    "reason": "rate_limit_exceeded"
                }
            }
        }"#;
        match parse_sse_data(data) {
            SseEvent::Error(msg) => {
                assert!(msg.contains("rate_limit_exceeded"));
                assert!(msg.contains("response.failed"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn parse_ignored_events() {
        let ignored_types = [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_item.done",
            "response.content_part.added",
            "response.content_part.done",
            "response.output_text.done",
            "response.function_call_arguments.delta",
        ];

        for event_type in &ignored_types {
            let data = format!(r#"{{"type":"{}"}}"#, event_type);
            assert!(
                matches!(parse_sse_data(&data), SseEvent::Ignored),
                "{event_type} should be Ignored"
            );
        }
    }

    #[test]
    fn build_request_includes_tools_and_system() {
        let tools = vec![serde_json::json!({"type": "function", "name": "test", "parameters": {}})];
        let input = user_message_input("hello");
        let body = build_request_body("gpt-5.4", &input, &tools, Some("You are helpful"), None, None);

        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are helpful");
        assert!(body["tools"].as_array().unwrap().len() == 1);
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["type"], "response.create");
    }

    #[test]
    fn build_request_with_previous_response_id() {
        let input = user_message_input("continue");
        let body = build_request_body("gpt-5.4", &input, &[], None, Some("resp_abc"), None);

        assert_eq!(body["previous_response_id"], "resp_abc");
    }

    #[test]
    fn build_request_with_reasoning_effort() {
        let input = user_message_input("think hard");
        let body = build_request_body("gpt-5.4", &input, &[], None, None, Some("low"));

        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn build_request_without_reasoning_effort() {
        let input = user_message_input("quick");
        let body = build_request_body("gpt-5.4", &input, &[], None, None, None);

        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn tool_result_input_format() {
        let input = tool_result_input("call_123", "action succeeded");
        let arr = input.as_array().unwrap();
        assert_eq!(arr[0]["type"], "function_call_output");
        assert_eq!(arr[0]["call_id"], "call_123");
        assert_eq!(arr[0]["output"], "action succeeded");
    }

    #[test]
    fn compile_tools_from_game_set() {
        let tools = compile_tools(&["game".to_string()]);
        assert!(!tools.is_empty());
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "game_action");
    }

    #[test]
    fn compile_tools_from_system_set() {
        let tools = compile_tools(&["system".to_string()]);
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"query_world_state"));
        assert!(names.contains(&"approve"));
        assert!(names.contains(&"reject"));
    }

    #[test]
    fn cost_estimation() {
        let cost = estimate_cost(1_000_000, 1_000_000);
        // $2.50 input + $10.00 output = $12.50
        assert!((cost - 12.50).abs() < 0.01);
    }

    #[test]
    fn adapter_for_agent_sets_label() {
        let adapter = OpenAiAdapter::for_agent("Alice", "uuid-123", "gpt-5.4", String::new(), vec![]);
        assert_eq!(adapter.label, "openai:Alice");
        assert_eq!(adapter.model, "gpt-5.4");
        assert_eq!(adapter.agent_identity, Some(("Alice".to_string(), "uuid-123".to_string())));
        assert_eq!(adapter.reasoning_effort, Some("low".to_string()));
    }

    #[test]
    fn adapter_for_system_ai_sets_label() {
        let adapter = OpenAiAdapter::for_system_ai("gpt-5.4", String::new(), vec![]);
        assert_eq!(adapter.label, "openai:system-ai");
        assert_eq!(adapter.reasoning_effort, Some("low".to_string()));
    }

    #[test]
    fn derive_ws_url_from_http_base() {
        assert_eq!(
            derive_ws_url("https://api.openai.com/v1"),
            "wss://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn derive_ws_url_from_wss_base() {
        assert_eq!(
            derive_ws_url("wss://api.openai.com/v1/responses"),
            "wss://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn derive_ws_url_from_wss_base_without_path() {
        assert_eq!(
            derive_ws_url("wss://api.openai.com/v1"),
            "wss://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_from_url("wss://api.openai.com/v1/responses"), "api.openai.com");
        assert_eq!(host_from_url("wss://custom.host:8080/v1/responses"), "custom.host:8080");
    }
}
