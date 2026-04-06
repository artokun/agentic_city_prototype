//! Tool execution entrypoints.
//! Routes tool calls to the game server and returns normalized ToolCallResult.
//! Used by both MCP stdio wrappers and future in-process adapters.

use serde_json::{json, Value};

use crate::llm::types::ToolCallResult;

const DEFAULT_GAME_SERVER: &str = "http://127.0.0.1:8080";

fn game_server_url() -> String {
    std::env::var("GAME_SERVER_URL").unwrap_or_else(|_| DEFAULT_GAME_SERVER.to_string())
}

/// Execute a game-agent tool call (the `game_action` tool).
/// Injects agent identity and forwards to the game server via HTTP.
pub fn execute_game_action(
    arguments: &Value,
    agent_name: &str,
    agent_id: &str,
) -> ToolCallResult {
    let mut args = arguments.clone();
    args["agent_name"] = json!(agent_name);
    args["agent_id"] = json!(agent_id);

    let result = forward_post(&format!("{}/api/action", game_server_url()), &args);
    match result {
        Ok(response) => ToolCallResult {
            id: String::new(), // caller sets this
            output: response,
            is_error: false,
        },
        Err(e) => ToolCallResult {
            id: String::new(),
            output: format!("Game server error: {e}"),
            is_error: true,
        },
    }
}

/// Execute a system-AI tool call by name.
pub fn execute_system_tool(tool_name: &str, arguments: &Value) -> ToolCallResult {
    let base = game_server_url();
    let result = match tool_name {
        "query_world_state" => {
            let query = arguments
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("");
            execute_query_world_state(query, &base)
        }
        "read_document" => {
            let agent_name = arguments
                .get("agent_name")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let title = arguments
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            execute_read_document(agent_name, title, &base)
        }
        "approve" => {
            let bounty_id = arguments
                .get("bounty_id")
                .and_then(|b| b.as_str())
                .unwrap_or("");
            let message = arguments
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("approved");
            execute_verdict(bounty_id, true, message, &base)
        }
        "reject" => {
            let bounty_id = arguments
                .get("bounty_id")
                .and_then(|b| b.as_str())
                .unwrap_or("");
            let message = arguments
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("rejected");
            execute_verdict(bounty_id, false, message, &base)
        }
        "grant_gold" => {
            let body = json!({
                "agent_name": arguments.get("agent_name").and_then(|a| a.as_str()).unwrap_or(""),
                "amount": arguments.get("amount").and_then(|a| a.as_u64()).unwrap_or(0),
                "reason": arguments.get("reason").and_then(|r| r.as_str()).unwrap_or(""),
                "message": arguments.get("message").and_then(|m| m.as_str()).unwrap_or(""),
            });
            forward_post(&format!("{base}/api/gm/grant_gold"), &body)
        }
        _ => Err(format!("Unknown system tool: {tool_name}")),
    };

    match result {
        Ok(output) => ToolCallResult {
            id: String::new(),
            output,
            is_error: false,
        },
        Err(e) => ToolCallResult {
            id: String::new(),
            output: e,
            is_error: true,
        },
    }
}

// --- Internal helpers ---

const ALLOWED_QUERY_PREFIXES: [&str; 4] = ["agent:", "bounty:", "dropbox:", "structure:"];

fn execute_query_world_state(query: &str, base: &str) -> Result<String, String> {
    let query = query.trim();
    if query == "full" || query.is_empty() {
        return Err(
            "Focused queries only. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>.".into(),
        );
    }
    if !ALLOWED_QUERY_PREFIXES
        .iter()
        .any(|prefix| query.starts_with(prefix))
    {
        return Err(
            "Unsupported query. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>.".into(),
        );
    }

    let client = reqwest::blocking::Client::new();
    match client
        .get(format!("{base}/api/gm/query"))
        .query(&[("q", query)])
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                Ok(resp.text().unwrap_or_else(|_| "Empty response".into()))
            } else {
                Err(format!("Server error: {}", resp.status()))
            }
        }
        Err(e) => Err(format!("Connection error: {e}")),
    }
}

fn execute_read_document(agent_name: &str, title: &str, base: &str) -> Result<String, String> {
    let agent_name = agent_name.trim().to_ascii_lowercase();
    let title = title.trim();
    if agent_name.is_empty() || title.is_empty() {
        return Err("read_document requires non-empty agent_name and title.".into());
    }

    let mut url = reqwest::Url::parse(base).map_err(|e| format!("Invalid URL: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "Failed to build URL".to_string())?
        .extend(["api", "documents", &agent_name, title]);

    let client = reqwest::blocking::Client::new();
    match client.get(url).send() {
        Ok(resp) => {
            if resp.status().is_success() {
                Ok(resp.text().unwrap_or_else(|_| "Empty document".into()))
            } else {
                Err(format!("Server error: {}", resp.status()))
            }
        }
        Err(e) => Err(format!("Connection error: {e}")),
    }
}

fn execute_verdict(
    bounty_id: &str,
    approved: bool,
    reason: &str,
    base: &str,
) -> Result<String, String> {
    if bounty_id.trim().is_empty() {
        return Err("approve/reject requires a non-empty bounty_id.".into());
    }
    let body = json!({
        "bounty_id": bounty_id,
        "approved": approved,
        "reason": reason,
    });
    forward_post(&format!("{base}/api/gm/verdict"), &body)
}

/// POST JSON to the game server, extract the result string.
fn forward_post(url: &str, body: &Value) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(url)
        .json(body)
        .send()
        .map_err(|e| format!("HTTP error: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Server returned {}", resp.status()));
    }

    let body: Value = resp.json().map_err(|e| format!("JSON error: {e}"))?;

    // Game server returns { "result": "..." } for actions.
    body.get("result")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .or_else(|| serde_json::to_string_pretty(&body).ok())
        .ok_or_else(|| "Empty response".to_string())
}
