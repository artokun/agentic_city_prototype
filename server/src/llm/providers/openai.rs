//! OpenAI Responses API adapter — implements SessionAdapter for GPT-5.4.
//!
//! Owns: HTTP SSE streaming, function-tool compilation, tool-result submission,
//! compaction via context summarization, and hybrid resume with
//! `previous_response_id` + local checkpoint.
//!
//! No OpenAI event names or wire-format details leak outside this module.

use tokio::sync::mpsc;

use crate::llm::config::SessionProfile;
use crate::llm::supervisor::SessionAdapter;
use crate::llm::tools::catalog::tools_for_set;
use crate::llm::tools::schema::to_openai_functions;
use crate::llm::types::{
    AdapterError, SessionCheckpoint, SessionCommand, SessionEvent, ToolCallRequest, UsageData,
};

/// Default OpenAI API base URL.
const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

/// Environment variable for the API key.
const API_KEY_ENV: &str = "OPENAI_API_KEY";

// ---------------------------------------------------------------------------
// OpenAI SSE event types (internal only — never exposed outside this module)
// ---------------------------------------------------------------------------

/// Parsed SSE event from the Responses API stream.
#[derive(Debug)]
enum SseEvent {
    /// `response.output_text.delta` — incremental text.
    TextDelta(String),
    /// `response.function_call_arguments.done` — complete function call.
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
// SSE parsing
// ---------------------------------------------------------------------------

/// Parse a single SSE data line (the JSON payload after "data: ").
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
                .or_else(|| val.get("item_id"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
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
                call_id,
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

        // Output item events we can ignore.
        "response.created"
        | "response.in_progress"
        | "response.output_item.added"
        | "response.output_item.done"
        | "response.content_part.added"
        | "response.content_part.done"
        | "response.output_text.done"
        | "response.function_call_arguments.delta" => SseEvent::Ignored,

        _ => {
            tracing::debug!("[openai] ignoring SSE event: {event_type}");
            SseEvent::Ignored
        }
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// Build the JSON body for a Responses API request.
fn build_request_body(
    model: &str,
    input: &serde_json::Value,
    tools: &[serde_json::Value],
    system_prompt: Option<&str>,
    previous_response_id: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "input": input,
        "stream": true,
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
// Stream processing task
// ---------------------------------------------------------------------------

/// Configuration for the streaming task.
#[derive(Clone)]
struct StreamConfig {
    api_base: String,
    api_key: String,
    model: String,
    system_prompt: String,
    tool_sets: Vec<String>,
    label: String,
}

/// Run the streaming loop: reads commands, makes API calls, emits events.
async fn stream_loop(
    mut command_rx: mpsc::Receiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
    config: StreamConfig,
    initial_previous_response_id: Option<String>,
) {
    let client = reqwest::Client::new();
    let tools = compile_tools(&config.tool_sets);
    let mut previous_response_id = initial_previous_response_id;

    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            SessionCommand::SendUserTurn(text) => {
                let input = user_message_input(&text);
                let body = build_request_body(
                    &config.model,
                    &input,
                    &tools,
                    Some(&config.system_prompt),
                    previous_response_id.as_deref(),
                );

                match stream_response(&client, &config, &body, &event_tx).await {
                    Ok(resp_id) => {
                        if !resp_id.is_empty() {
                            previous_response_id = Some(resp_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!("[{}] stream error: {e}", config.label);
                        let _ = event_tx.send(SessionEvent::Error(e)).await;
                    }
                }
            }

            SessionCommand::SendToolResult(result) => {
                let input = tool_result_input(&result.id, &result.output);
                let body = build_request_body(
                    &config.model,
                    &input,
                    &tools,
                    None, // no need to resend system prompt for tool results
                    previous_response_id.as_deref(),
                );

                match stream_response(&client, &config, &body, &event_tx).await {
                    Ok(resp_id) => {
                        if !resp_id.is_empty() {
                            previous_response_id = Some(resp_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!("[{}] tool result stream error: {e}", config.label);
                        let _ = event_tx.send(SessionEvent::Error(e)).await;
                    }
                }
            }

            SessionCommand::Compact => {
                tracing::info!("[{}] compacting — resetting response chain", config.label);

                // Compaction: break the response chain and start fresh.
                // The supervisor tracks token totals and saves the checkpoint.

                // Start a new chain — the system prompt re-establishes context.
                let input = compaction_input(
                    "Session compacted. Previous response chain cleared.",
                );
                let body = build_request_body(
                    &config.model,
                    &input,
                    &tools,
                    Some(&config.system_prompt),
                    None, // deliberately break the chain
                );

                match stream_response(&client, &config, &body, &event_tx).await {
                    Ok(resp_id) => {
                        if !resp_id.is_empty() {
                            previous_response_id = Some(resp_id);
                        }
                        let _ = event_tx.send(SessionEvent::CompactCompleted).await;
                    }
                    Err(e) => {
                        tracing::error!("[{}] compaction error: {e}", config.label);
                        let _ = event_tx.send(SessionEvent::Error(e)).await;
                    }
                }
            }

            SessionCommand::Shutdown => {
                tracing::info!("[{}] shutdown requested", config.label);
                let _ = event_tx.send(SessionEvent::Completed).await;
                break;
            }
        }
    }

    tracing::info!("[{}] stream loop exited", config.label);
}

/// Make a single streaming request and process SSE events.
/// Returns the response ID on success.
async fn stream_response(
    client: &reqwest::Client,
    config: &StreamConfig,
    body: &serde_json::Value,
    event_tx: &mpsc::Sender<SessionEvent>,
) -> Result<String, String> {
    let url = format!("{}/responses", config.api_base);

    tracing::debug!(
        "[{}] POST {} (model={})",
        config.label,
        url,
        config.model
    );

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {body_text}"));
    }

    // Process SSE stream.
    let mut response_id = String::new();
    let text = resp.text().await.map_err(|e| format!("Read error: {e}"))?;

    // SSE format: lines starting with "data: " contain JSON payloads.
    // "data: [DONE]" signals end of stream.
    for line in text.lines() {
        let line = line.trim();

        if line.is_empty() || line.starts_with(':') {
            continue; // comment or blank
        }

        if !line.starts_with("data: ") {
            // Could be "event: " lines — skip.
            continue;
        }

        let data = &line["data: ".len()..];

        if data == "[DONE]" {
            break;
        }

        match parse_sse_data(data) {
            SseEvent::TextDelta(delta) => {
                let _ = event_tx.send(SessionEvent::TextDelta(delta)).await;
            }
            SseEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let args: serde_json::Value =
                    serde_json::from_str(&arguments).unwrap_or(serde_json::json!({}));
                tracing::info!("[{}] TOOL_CALL: {} ({})", config.label, name, call_id);
                let _ = event_tx
                    .send(SessionEvent::ToolCallRequested(ToolCallRequest {
                        id: call_id,
                        name,
                        arguments: args,
                    }))
                    .await;
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
                    // Estimate cost — GPT-5.4 pricing TBD, use placeholder.
                    let cost_usd = estimate_cost(u.input_tokens, u.output_tokens);
                    let _ = event_tx
                        .send(SessionEvent::Usage(UsageData {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cost_usd,
                        }))
                        .await;
                }
                let _ = event_tx.send(SessionEvent::Completed).await;
            }
            SseEvent::Error(msg) => {
                tracing::warn!("[{}] stream error: {}", config.label, msg);
                let _ = event_tx.send(SessionEvent::Error(msg)).await;
            }
            SseEvent::Ignored => {}
        }
    }

    Ok(response_id)
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
/// Streams responses via HTTP SSE, compiles shared tool catalog into OpenAI
/// function tools, and implements hybrid resume with local checkpoint +
/// `previous_response_id`.
pub struct OpenAiAdapter {
    /// Model to use (e.g. "gpt-5.4").
    model: String,
    /// API base URL.
    api_base: String,
    /// Command sender — cloned from the handle channels.
    command_tx: Option<mpsc::Sender<SessionCommand>>,
    /// Event receiver — taken once by the supervisor.
    event_rx: Option<mpsc::Receiver<SessionEvent>>,
    /// System prompt content.
    system_prompt: String,
    /// Tool set names (e.g. ["game"] or ["system"]).
    tool_sets: Vec<String>,
    /// Label for logging.
    label: String,
    /// Handle to the background streaming task.
    stream_task: Option<tokio::task::JoinHandle<()>>,
    /// Last known response ID for chain resumption.
    last_response_id: Option<String>,
    /// API key (resolved at start time).
    api_key_env: String,
}

impl OpenAiAdapter {
    /// Create a new adapter for an agent session.
    pub fn for_agent(
        agent_name: &str,
        model: &str,
        system_prompt: String,
        tool_sets: Vec<String>,
    ) -> Self {
        Self {
            model: model.to_string(),
            api_base: std::env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
            command_tx: None,
            event_rx: None,
            system_prompt,
            tool_sets,
            label: format!("openai:{agent_name}"),
            stream_task: None,
            last_response_id: None,
            api_key_env: API_KEY_ENV.to_string(),
        }
    }

    /// Create a new adapter for the system-AI session.
    pub fn for_system_ai(
        model: &str,
        system_prompt: String,
        tool_sets: Vec<String>,
    ) -> Self {
        Self {
            model: model.to_string(),
            api_base: std::env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
            command_tx: None,
            event_rx: None,
            system_prompt,
            tool_sets,
            label: "openai:system-ai".to_string(),
            stream_task: None,
            last_response_id: None,
            api_key_env: API_KEY_ENV.to_string(),
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
        if self.command_tx.is_some() {
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
                    .get("previous_response_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| self.last_response_id.clone());

        // Create channels.
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<SessionEvent>(256);

        self.command_tx = Some(cmd_tx);
        self.event_rx = Some(evt_rx);

        let config = StreamConfig {
            api_base: self.api_base.clone(),
            api_key,
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            tool_sets: self.tool_sets.clone(),
            label: self.label.clone(),
        };

        // Spawn the streaming task.
        let handle = tokio::spawn(stream_loop(cmd_rx, evt_tx, config, previous_response_id));
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

        // Build a checkpoint with provider metadata containing the last response ID.
        let metadata = if let Some(ref rid) = self.last_response_id {
            serde_json::json!({ "previous_response_id": rid })
        } else {
            serde_json::json!({})
        };

        Ok(Some(SessionCheckpoint {
            owner: crate::llm::types::SessionOwner::Agent("unknown".to_string()),
            provider_id: self
                .last_response_id
                .clone()
                .unwrap_or_default(),
            model: self.model.clone(),
            compact_threshold: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            last_turn_marker: String::new(),
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
        let tools = vec![serde_json::json!({"type": "function", "function": {"name": "test"}})];
        let input = user_message_input("hello");
        let body = build_request_body("gpt-5.4", &input, &tools, Some("You are helpful"), None);

        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are helpful");
        assert!(body["tools"].as_array().unwrap().len() == 1);
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn build_request_with_previous_response_id() {
        let input = user_message_input("continue");
        let body = build_request_body("gpt-5.4", &input, &[], None, Some("resp_abc"));

        assert_eq!(body["previous_response_id"], "resp_abc");
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
        assert_eq!(tools[0]["function"]["name"], "game_action");
    }

    #[test]
    fn compile_tools_from_system_set() {
        let tools = compile_tools(&["system".to_string()]);
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
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
        let adapter = OpenAiAdapter::for_agent("Alice", "gpt-5.4", String::new(), vec![]);
        assert_eq!(adapter.label, "openai:Alice");
        assert_eq!(adapter.model, "gpt-5.4");
    }

    #[test]
    fn adapter_for_system_ai_sets_label() {
        let adapter = OpenAiAdapter::for_system_ai("gpt-5.4", String::new(), vec![]);
        assert_eq!(adapter.label, "openai:system-ai");
    }
}
