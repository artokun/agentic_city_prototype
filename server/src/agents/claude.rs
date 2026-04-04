use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Manages a Claude CLI process for one agent.
pub struct ClaudeSession {
    /// Send prompts to Claude.
    pub prompt_tx: mpsc::Sender<String>,
    /// Receive NDJSON responses from Claude.
    pub response_rx: mpsc::Receiver<serde_json::Value>,
    pub session_id: Option<String>,
}

/// Spawn a Claude CLI process for an agent.
pub async fn spawn_claude_session(
    agent_name: &str,
    system_prompt: &str,
) -> Result<ClaudeSession, String> {
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(16);
    let (response_tx, response_rx) = mpsc::channel::<serde_json::Value>(64);

    let agent_name = agent_name.to_string();
    let system_prompt = system_prompt.to_string();

    tokio::spawn(async move {
        // Build environment — remove API key to use subscription auth.
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("ANTHROPIC_API_KEY");

        let mut child = match Command::new("claude")
            .args([
                "--output-format", "stream-json",
                "--input-format", "stream-json",
                "--permission-mode", "bypassPermissions",
                "--model", "haiku",
                "--verbose",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(&env)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                tracing::error!("Failed to spawn claude for {}: {}", agent_name, e);
                return;
            }
        };

        tracing::info!("Claude session started for {}", agent_name);

        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        // Log stderr in background.
        let agent_name_err = agent_name.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.is_empty() {
                    tracing::debug!("[claude:{}:stderr] {}", agent_name_err, line);
                }
            }
        });

        // Read stdout NDJSON in background.
        let agent_name_out = agent_name.clone();
        let resp_tx = response_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(msg) => {
                        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
                        tracing::debug!("[claude:{}] {} message", agent_name_out, msg_type);

                        // Auto-approve control requests.
                        if msg_type == "control_request" {
                            // We'd need to write back to stdin, but we handle this
                            // via --permission-mode bypassPermissions
                        }

                        if let Err(_) = resp_tx.send(msg).await {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("[claude:{}] non-JSON: {}", agent_name_out, line);
                    }
                }
            }
        });

        // Send initial system prompt.
        let init_msg = serde_json::json!({
            "type": "user",
            "content": format!(
                "{}\n\nYou are {}. Respond with a JSON action object. Do not include any other text.",
                system_prompt, agent_name
            ),
        });
        let init_line = format!("{}\n", serde_json::to_string(&init_msg).unwrap());
        let _ = stdin.write_all(init_line.as_bytes()).await;
        let _ = stdin.flush().await;

        // Forward prompts from the game to Claude's stdin.
        while let Some(prompt) = prompt_rx.recv().await {
            let msg = serde_json::json!({
                "type": "user",
                "content": prompt,
            });
            let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
            if stdin.write_all(line.as_bytes()).await.is_err() {
                tracing::warn!("Claude stdin closed for {}", agent_name);
                break;
            }
            let _ = stdin.flush().await;
        }

        tracing::info!("Claude session ended for {}", agent_name);
    });

    Ok(ClaudeSession {
        prompt_tx,
        response_rx,
        session_id: None,
    })
}

/// Extract the assistant's text response from NDJSON messages.
/// Drains all available messages and returns the last text content.
pub fn drain_response(rx: &mut mpsc::Receiver<serde_json::Value>) -> Option<String> {
    let mut last_text = None;

    while let Ok(msg) = rx.try_recv() {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match msg_type {
            "assistant" => {
                // Look for text content blocks.
                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    last_text = Some(text.to_string());
                                }
                            }
                        }
                    }
                }
                // Single message format.
                if let Some(message) = msg.get("message") {
                    if let Some(content) = message.get("content") {
                        if let Some(arr) = content.as_array() {
                            for block in arr {
                                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        last_text = Some(text.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "result" => {
                // Final result message.
                if let Some(result) = msg.get("result") {
                    if let Some(text) = result.as_str() {
                        last_text = Some(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    last_text
}
