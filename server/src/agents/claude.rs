use tokio::sync::mpsc;

/// Channels for communicating with a Claude session over --sdk-url.
pub struct ClaudeSession {
    /// Send user messages to Claude (game engine → agent).
    pub prompt_tx: mpsc::Sender<String>,
    /// Receive text responses from Claude (agent → game engine).
    pub response_rx: mpsc::Receiver<String>,
}

/// Format a user message in the NDJSON protocol Claude CLI expects.
pub fn format_user_message(text: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "session_id": "",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": text,
                }
            ]
        },
        "parent_tool_use_id": null,
    });
    format!("{}\n", serde_json::to_string(&msg).unwrap())
}

/// Format a control_response to approve a tool use request.
pub fn format_control_response(request_id: &str, tool_use_id: Option<&str>, input: Option<&serde_json::Value>) -> String {
    let mut response = serde_json::json!({
        "subtype": "success",
        "request_id": request_id,
        "response": {},
    });

    // For tool use requests, include behavior: allow.
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

/// Extract the final text from a result message.
pub fn extract_result_text(msg: &serde_json::Value) -> Option<String> {
    let msg_type = msg.get("type")?.as_str()?;

    match msg_type {
        "result" => {
            msg.get("result").and_then(|r| r.as_str()).map(|s| s.to_string())
        }
        "assistant" => {
            // Look for text content blocks in the message.
            let content = msg.get("message")
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
