//! Claude CLI adapter — implements SessionAdapter for `claude --sdk-url`.
//!
//! Owns: process spawning, relay attachment, NDJSON framing, control-request
//! approval, MCP config generation, compaction, and PID lifecycle.
//! No code outside this module should emit Claude NDJSON or manage Claude processes.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Notify};

use crate::llm::config::SessionProfile;
use crate::llm::supervisor::SessionAdapter;
use crate::llm::types::{
    AdapterError, SessionCheckpoint, SessionCommand, SessionEvent, UsageData,
};
use crate::process_manager::ProcessRegistry;

// ---------------------------------------------------------------------------
// NDJSON framing helpers (moved from agents/claude.rs)
// ---------------------------------------------------------------------------

/// Format a user message in the NDJSON protocol Claude CLI expects.
fn format_user_message(text: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "session_id": "",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }]
        },
        "parent_tool_use_id": null,
    });
    format!("{}\n", serde_json::to_string(&msg).unwrap())
}

/// Format a control_response to approve a tool use request.
fn format_control_response(
    request_id: &str,
    tool_use_id: Option<&str>,
    input: Option<&serde_json::Value>,
) -> String {
    let mut response = serde_json::json!({
        "subtype": "success",
        "request_id": request_id,
        "response": {},
    });

    if let Some(tool_id) = tool_use_id {
        response["response"] = serde_json::json!({
            "behavior": "allow",
            "toolUseID": tool_id,
        });
        if let Some(inp) = input {
            response["response"]["updatedInput"] = inp.clone();
        }
    }

    let msg = serde_json::json!({
        "type": "control_response",
        "response": response,
    });
    format!("{}\n", serde_json::to_string(&msg).unwrap())
}

/// Extract text from a result or assistant NDJSON message.
fn extract_result_text(msg: &serde_json::Value) -> Option<String> {
    let msg_type = msg.get("type")?.as_str()?;
    match msg_type {
        "result" => msg
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string()),
        "assistant" => {
            let content = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| msg.get("content"))?;
            if let Some(arr) = content.as_array() {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            return Some(text.to_string());
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// MCP config generation
// ---------------------------------------------------------------------------

/// Resolve a sibling binary path relative to the current executable.
fn resolve_sibling_binary(binary_name: &str) -> String {
    std::env::current_exe()
        .map(|exe| {
            let dir = exe.parent().unwrap();
            let candidate = dir.join(binary_name);
            if candidate.exists() {
                candidate.to_string_lossy().to_string()
            } else if let Some(parent) = dir.parent() {
                parent.join(binary_name).to_string_lossy().to_string()
            } else {
                candidate.to_string_lossy().to_string()
            }
        })
        .unwrap_or_else(|_| binary_name.to_string())
}

/// Write agent MCP config (game_action tool with identity baked in).
fn write_agent_mcp_config(agent_name: &str, agent_uuid: &str) -> String {
    let mcp_binary = resolve_sibling_binary("mcp-game");
    let path = format!("/tmp/mcp-{}.json", agent_uuid);
    let config = serde_json::json!({
        "mcpServers": {
            "game-engine": {
                "command": mcp_binary,
                "args": [agent_name, agent_uuid],
                "env": {
                    "AGENT_NAME": agent_name,
                    "AGENT_ID": agent_uuid,
                }
            }
        }
    });
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
    path
}

/// Write system-AI MCP config (GM tools).
fn write_system_mcp_config() -> String {
    let mcp_binary = resolve_sibling_binary("mcp-gm");
    let path = "/tmp/mcp-system-ai.json".to_string();
    let config = serde_json::json!({
        "mcpServers": {
            "system-ai": {
                "command": mcp_binary,
                "args": [],
            }
        }
    });
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
    path
}

// ---------------------------------------------------------------------------
// Relay event translation
// ---------------------------------------------------------------------------

/// Configuration for how the relay loop should behave for different session roles.
#[derive(Clone)]
pub struct RelayConfig {
    /// Human-readable label for log lines.
    pub label: String,
    /// Whether to forward assistant thinking text as SessionEvent::TextDelta.
    pub forward_thoughts: bool,
}

/// Run the relay loop: bridges a WebSocket connection to SessionEvent/SessionCommand channels.
///
/// Reads commands from `command_rx`, formats them as NDJSON, sends over the socket.
/// Reads NDJSON from the socket, translates to SessionEvent, sends via `event_tx`.
async fn relay_loop(
    mut socket: axum::extract::ws::WebSocket,
    mut command_rx: mpsc::Receiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
    config: RelayConfig,
) {
    use axum::extract::ws::Message;

    loop {
        tokio::select! {
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    SessionCommand::SendUserTurn(text) => {
                        let ndjson = format_user_message(&text);
                        tracing::debug!("[{}] -> Claude: user message ({}b)", config.label, ndjson.len());
                        if socket.send(Message::Text(ndjson.into())).await.is_err() {
                            tracing::warn!("[{}] WebSocket send failed", config.label);
                            break;
                        }
                    }
                    SessionCommand::Compact => {
                        let ndjson = format_user_message("/compact");
                        tracing::info!("[{}] -> Claude: /compact", config.label);
                        if socket.send(Message::Text(ndjson.into())).await.is_err() {
                            break;
                        }
                    }
                    SessionCommand::Shutdown => {
                        tracing::info!("[{}] shutdown requested", config.label);
                        let _ = event_tx.send(SessionEvent::Completed).await;
                        break;
                    }
                    SessionCommand::SendToolResult(_) => {
                        // Tool results are handled by the MCP binary, not the relay.
                    }
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        process_ndjson_lines(&text, &event_tx, &config, &mut socket).await;
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            process_ndjson_lines(&text, &event_tx, &config, &mut socket).await;
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong
                    _ => {
                        tracing::info!("[{}] Claude disconnected", config.label);
                        break;
                    }
                }
            }
        }
    }

    let _ = event_tx.send(SessionEvent::Error("session disconnected".into())).await;
}

/// Process NDJSON lines from Claude, translate to SessionEvent values.
async fn process_ndjson_lines(
    text: &str,
    event_tx: &mpsc::Sender<SessionEvent>,
    config: &RelayConfig,
    socket: &mut axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let msg_type = val
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("?");

        match msg_type {
            "control_request" => {
                let request_id = val
                    .get("request_id")
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let tool_use_id = val
                    .get("request")
                    .and_then(|r| r.get("tool_use_id"))
                    .and_then(|t| t.as_str());
                let input = val
                    .get("request")
                    .and_then(|r| r.get("input"));

                let response = format_control_response(request_id, tool_use_id, input);
                if socket.send(Message::Text(response.into())).await.is_err() {
                    return;
                }
            }

            "result" => {
                // Extract token usage.
                let input_tokens = val
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let output_tokens = val
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let cost_usd = val
                    .get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                if input_tokens > 0 || output_tokens > 0 {
                    tracing::info!(
                        "[{}] tokens: in={}, out={}, cost=${:.4}",
                        config.label,
                        input_tokens,
                        output_tokens,
                        cost_usd
                    );
                    let _ = event_tx
                        .send(SessionEvent::Usage(UsageData {
                            input_tokens,
                            output_tokens,
                            cost_usd,
                        }))
                        .await;
                }

                // Extract result text.
                if let Some(text) = extract_result_text(&val) {
                    let preview: String = text.chars().take(80).collect();
                    tracing::info!("[{}] response: {}...", config.label, preview);
                    let _ = event_tx.send(SessionEvent::Completed).await;
                }
            }

            "assistant" => {
                if config.forward_thoughts {
                    if let Some(text) = extract_result_text(&val) {
                        if !text.is_empty() {
                            let preview: String = text.chars().take(100).collect();
                            tracing::info!("[{}] THOUGHT: {}", config.label, preview);
                            let _ = event_tx
                                .send(SessionEvent::TextDelta(format!("thought:{}", text)))
                                .await;
                        }
                    }
                }

                // Log tool use calls.
                let content = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .or_else(|| val.get("content"));
                if let Some(arr) = content.and_then(|c| c.as_array()) {
                    for block in arr {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                        if btype == "tool_use" {
                            let tool = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            tracing::info!("[{}] TOOL_USE: {}", config.label, tool);
                        }
                    }
                }
            }

            "system" => {} // skip silently
            _ => {
                tracing::debug!("[{}] {} message", config.label, msg_type);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeAdapter
// ---------------------------------------------------------------------------

/// Claude CLI session adapter.
///
/// Spawns `claude --sdk-url`, connects via WebSocket relay, and translates
/// between the unified SessionCommand/SessionEvent protocol and Claude NDJSON.
pub struct ClaudeAdapter {
    /// Model to use (e.g. "haiku", "opus", "sonnet").
    model: String,
    /// Command sender — cloned from the handle channels.
    command_tx: Option<mpsc::Sender<SessionCommand>>,
    /// Event receiver — taken once by the supervisor.
    event_rx: Option<mpsc::Receiver<SessionEvent>>,
    /// Process ID of the spawned Claude CLI.
    child_pid: Option<u32>,
    /// Shared process registry for clean shutdown.
    process_registry: ProcessRegistry,
    /// Temp files to clean up on shutdown.
    temp_files: Vec<String>,
    /// WebSocket port for --sdk-url.
    ws_port: u16,
    /// Agent identity (for agent sessions).
    agent_identity: Option<AgentIdentity>,
    /// Whether this is a system-AI session (different relay + MCP config).
    is_system_ai: bool,
    /// System prompt content.
    system_prompt: String,
    /// Whether to forward assistant thinking as TextDelta events.
    forward_thoughts: bool,
    /// Connection notification.
    connected: Option<Arc<Notify>>,
    /// Label for logging.
    label: String,
}

/// Identity info for agent sessions (not needed for system-AI).
#[derive(Clone)]
pub struct AgentIdentity {
    pub name: String,
    pub uuid: String,
}

impl ClaudeAdapter {
    /// Create a new adapter for an agent session.
    pub fn for_agent(
        identity: AgentIdentity,
        model: &str,
        system_prompt: String,
        ws_port: u16,
        process_registry: ProcessRegistry,
    ) -> Self {
        Self {
            model: model.to_string(),
            command_tx: None,
            event_rx: None,
            child_pid: None,
            process_registry,
            temp_files: Vec::new(),
            ws_port,
            agent_identity: Some(identity.clone()),
            is_system_ai: false,
            system_prompt,
            forward_thoughts: true,
            connected: None,
            label: format!("claude:{}", identity.name),
        }
    }

    /// Create a new adapter for the system-AI session.
    pub fn for_system_ai(
        model: &str,
        system_prompt: String,
        ws_port: u16,
        process_registry: ProcessRegistry,
    ) -> Self {
        Self {
            model: model.to_string(),
            command_tx: None,
            event_rx: None,
            child_pid: None,
            process_registry,
            temp_files: Vec::new(),
            ws_port,
            agent_identity: None,
            is_system_ai: true,
            system_prompt,
            forward_thoughts: false,
            connected: None,
            label: "claude:system-ai".to_string(),
        }
    }

    /// Spawn the Claude CLI process and wire it to the relay.
    async fn spawn_process(&mut self) -> Result<(), AdapterError> {
        // Write system prompt to temp file.
        let prompt_file = if self.is_system_ai {
            "/tmp/system-ai-prompt.md".to_string()
        } else {
            let uuid = self
                .agent_identity
                .as_ref()
                .map(|i| i.uuid.as_str())
                .unwrap_or("unknown");
            format!("/tmp/agent-{}.md", uuid)
        };
        tokio::fs::write(&prompt_file, &self.system_prompt)
            .await
            .map_err(|e| AdapterError::Provider(format!("Failed to write prompt file: {e}")))?;
        self.temp_files.push(prompt_file.clone());

        // Write MCP config.
        let mcp_config_path = if self.is_system_ai {
            write_system_mcp_config()
        } else {
            let id = self.agent_identity.as_ref().unwrap();
            write_agent_mcp_config(&id.name, &id.uuid)
        };
        self.temp_files.push(mcp_config_path.clone());

        // Build --sdk-url.
        let sdk_url = if self.is_system_ai {
            format!("ws://127.0.0.1:{}/system/ws", self.ws_port)
        } else {
            let uuid = &self.agent_identity.as_ref().unwrap().uuid;
            format!("ws://127.0.0.1:{}/agent/{}/ws", self.ws_port, uuid)
        };

        // Build CLI args.
        let mut args = vec![
            "--sdk-url".to_string(),
            sdk_url.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
            "--model".to_string(),
            self.model.clone(),
            "--append-system-prompt-file".to_string(),
            prompt_file.clone(),
            "--mcp-config".to_string(),
            mcp_config_path.clone(),
            "--verbose".to_string(),
        ];

        // System AI gets a settings file for autoCompactWindow.
        if self.is_system_ai {
            let settings_path = "/tmp/system-ai-settings.json".to_string();
            let settings = serde_json::json!({
                "autoCompactWindow": crate::config::system_ai_compact_limit(),
            });
            tokio::fs::write(
                &settings_path,
                serde_json::to_string_pretty(&settings).unwrap(),
            )
            .await
            .map_err(|e| {
                AdapterError::Provider(format!("Failed to write settings file: {e}"))
            })?;
            self.temp_files.push(settings_path.clone());
            args.push("--settings".to_string());
            args.push(settings_path);
        }

        // Spawn the process.
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("ANTHROPIC_API_KEY");

        let mut child = tokio::process::Command::new("claude")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(&env)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AdapterError::Provider(format!("Failed to spawn claude: {e}")))?;

        let pid = child.id().unwrap_or(0);
        if pid > 0 {
            self.process_registry.register(pid);
            self.child_pid = Some(pid);
        }

        tracing::info!(
            "[{}] Claude spawned -> {} (model: {}, pid: {})",
            self.label,
            sdk_url,
            self.model,
            pid
        );

        // Capture stdout for debug logging.
        let label_out = self.label.clone();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::debug!("[{}:stdout] {}", label_out, &line[..line.len().min(200)]);
                    }
                }
            });
        }

        // Capture stderr for debug logging.
        let label_err = self.label.clone();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.is_empty() && !line.contains("debugger") {
                        tracing::debug!("[{}:stderr] {}", label_err, line);
                    }
                }
            });
        }

        // Wait for exit in background and clean up.
        let process_registry = self.process_registry.clone();
        let label_wait = self.label.clone();
        let temp_files = self.temp_files.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            if pid > 0 {
                process_registry.remove(pid);
            }
            tracing::warn!("[{}] Claude process exited (pid: {})", label_wait, pid);
            // Clean up temp files.
            for f in &temp_files {
                let _ = tokio::fs::remove_file(f).await;
            }
        });

        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionAdapter for ClaudeAdapter {
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

        // If resuming from checkpoint, prepend compacted context to the system prompt.
        if let Some(cp) = checkpoint {
            if let Some(ref ctx) = cp.compacted_context {
                tracing::info!(
                    "[{}] resuming from checkpoint (model={}, tokens_in={})",
                    self.label,
                    cp.model,
                    cp.total_input_tokens,
                );
                self.system_prompt = format!(
                    "## Previous Context (compacted)\n{}\n\n---\n\n{}",
                    ctx, self.system_prompt
                );
            }
        }

        // Create channels.
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<SessionEvent>(256);

        self.command_tx = Some(cmd_tx);
        self.event_rx = Some(evt_rx);

        // Spawn the CLI process.
        self.spawn_process().await?;

        // The relay loop will be started when the WebSocket handler calls into us.
        // For now, store channels for the relay to pick up.
        // The relay is started by the existing agent_relay/system_relay WebSocket handlers.
        // Those handlers will be updated to use SessionEvent channels.

        // Store the channels for relay_loop to use.
        // The relay handlers need access to these — we'll wire that up via
        // the existing AgentRelays / SystemRelay infrastructure which stays for now.

        // Since the relay modules stay as Claude transport infrastructure (per design guidance),
        // we don't start relay_loop here. The existing relay handlers already do the
        // WebSocket connection. We just need to bridge the channels.

        // For now this is a stub — the actual channel wiring happens in the
        // spawn_sessions_system / spawn_system_ai_session_system refactored code.
        let _ = (cmd_rx, evt_tx);

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
        // Kill the process if still running.
        if let Some(pid) = self.child_pid.take() {
            self.process_registry.remove(pid);
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        self.command_tx = None;
        self.event_rx = None;

        // Clean up temp files.
        for f in self.temp_files.drain(..) {
            let _ = tokio::fs::remove_file(&f).await;
        }

        // Build owner from adapter state.
        let owner = if self.is_system_ai {
            crate::llm::types::SessionOwner::SystemAi
        } else if let Some(ref id) = self.agent_identity {
            crate::llm::types::SessionOwner::Agent(id.name.clone())
        } else {
            crate::llm::types::SessionOwner::Agent("unknown".to_string())
        };

        // Return a checkpoint so the supervisor can persist it.
        // Token totals are tracked by the supervisor from Usage events,
        // so we just provide the structural fields here.
        Ok(Some(SessionCheckpoint {
            owner,
            provider_id: self.agent_identity.as_ref().map(|id| id.uuid.clone()),
            model: self.model.clone(),
            compact_threshold: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            last_turn_marker: None,
            compacted_context: None,
            provider_metadata: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// Standalone process spawning — used by ai.rs and gm.rs during transition
// ---------------------------------------------------------------------------

/// Spawn a Claude CLI process for an agent session.
/// The process connects back via WebSocket at `ws://127.0.0.1:{port}/agent/{uuid}/ws`.
/// Returns Ok(()) on successful spawn, Err on failure.
/// The process runs in the background and registers its PID for clean shutdown.
pub async fn spawn_agent_process(
    identity: &AgentIdentity,
    model: &str,
    system_prompt: &str,
    ws_port: u16,
    process_registry: &ProcessRegistry,
) -> Result<(), String> {
    let mut adapter = ClaudeAdapter::for_agent(
        identity.clone(),
        model,
        system_prompt.to_string(),
        ws_port,
        process_registry.clone(),
    );
    adapter
        .spawn_process()
        .await
        .map_err(|e| e.to_string())
}

/// Spawn a Claude CLI process for the system-AI (Game Master) session.
/// The process connects back via WebSocket at `ws://127.0.0.1:{port}/system/ws`.
/// Returns Ok(()) on successful spawn, Err on failure.
pub async fn spawn_system_ai_process(
    model: &str,
    system_prompt: &str,
    ws_port: u16,
    process_registry: &ProcessRegistry,
) -> Result<(), String> {
    let mut adapter = ClaudeAdapter::for_system_ai(
        model,
        system_prompt.to_string(),
        ws_port,
        process_registry.clone(),
    );
    adapter
        .spawn_process()
        .await
        .map_err(|e| e.to_string())
}

// Re-export NDJSON helpers for relay modules that still need them during transition.
// These are the canonical implementations — agent_relay.rs and system_relay.rs
// import from here instead of agents/claude.rs.
pub use self::ndjson::*;

mod ndjson {
    /// Format a user message in the Claude NDJSON protocol.
    pub fn claude_format_user_message(text: &str) -> String {
        super::format_user_message(text)
    }

    /// Format a control_response to approve a tool use request.
    pub fn claude_format_control_response(
        request_id: &str,
        tool_use_id: Option<&str>,
        input: Option<&serde_json::Value>,
    ) -> String {
        super::format_control_response(request_id, tool_use_id, input)
    }

    /// Extract text from a result or assistant NDJSON message.
    pub fn claude_extract_result_text(msg: &serde_json::Value) -> Option<String> {
        super::extract_result_text(msg)
    }
}
