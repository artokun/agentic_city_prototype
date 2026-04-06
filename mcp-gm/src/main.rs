//! MCP System AI server — thin stdio MCP wrapper over the shared tool runtime.
//! Used by the persistent System AI session to verify bounty submissions.
//!
//! Document inspection tracking (must read docs before verdict) is process-local
//! state managed here, wrapping the shared execution layer.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::{LazyLock, Mutex};

use server::llm::tools::catalog::tools_for_set;
use server::llm::tools::execute::execute_system_tool;
use server::llm::tools::schema::to_mcp_tools_list;

const GAME_SERVER: &str = "http://127.0.0.1:8080";
static INSPECTED_DOCUMENTS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }

        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        let response = match method {
            "initialize" => handle_initialize(&id),
            "tools/list" => handle_tools_list(&id),
            "tools/call" => handle_tool_call(&msg, &id),
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => json_rpc_error(&id, -32601, &format!("Unknown method: {}", method)),
        };

        let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
        let _ = stdout.flush();
    }
}

fn handle_initialize(id: &Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "system-ai", "version": "0.1.0" }
        }
    })
}

fn handle_tools_list(id: &Option<Value>) -> Value {
    let tools = tools_for_set("system");
    let result = to_mcp_tools_list(&tools);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn handle_tool_call(msg: &Value, id: &Option<Value>) -> Value {
    let tool_name = msg
        .pointer("/params/name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let arguments = msg
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(json!({}));

    let result = match tool_name {
        "read_document" => {
            // Wrap read_document to track inspection state.
            let agent_name = arguments
                .get("agent_name")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let title = arguments
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("");

            // Try focused state first, then fall back to shared execute.
            let doc_result = try_read_from_focused_state(agent_name, title)
                .unwrap_or_else(|| {
                    let r = execute_system_tool(tool_name, &arguments);
                    r.output
                });

            mark_document_inspected(agent_name, title, doc_result.len());
            doc_result
        }
        "approve" | "reject" => {
            let bounty_id = arguments
                .get("bounty_id")
                .and_then(|b| b.as_str())
                .unwrap_or("");
            let approved = tool_name == "approve";

            // Pre-verdict validation.
            if approved {
                if let Err(err) = validate_approval_state(bounty_id) {
                    return tool_result(id, &err);
                }
            }
            let keys_to_clear = match required_document_keys(bounty_id) {
                Ok(keys) => keys,
                Err(err) => return tool_result(id, &err),
            };
            if let Err(err) = validate_document_inspection(&keys_to_clear) {
                return tool_result(id, &err);
            }

            let r = execute_system_tool(tool_name, &arguments);
            if !r.is_error {
                clear_document_inspection(&keys_to_clear);
            }
            r.output
        }
        "query_world_state" | "grant_gold" => {
            let r = execute_system_tool(tool_name, &arguments);
            r.output
        }
        _ => format!("Unknown tool: {}", tool_name),
    };

    tool_result(id, &result)
}

fn tool_result(id: &Option<Value>, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }]
        }
    })
}

fn json_rpc_error(id: &Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

// ---------------------------------------------------------------------------
// Document inspection tracking (process-local state)
// ---------------------------------------------------------------------------

fn mark_document_inspected(agent_name: &str, title: &str, content_length: usize) {
    if let Ok(mut inspected) = INSPECTED_DOCUMENTS.lock() {
        inspected.insert(document_key(agent_name, title, content_length));
    }
}

fn document_key(agent_name: &str, title: &str, content_length: usize) -> String {
    format!(
        "{}::{}::{}",
        agent_name.to_ascii_lowercase(),
        title,
        content_length
    )
}

fn clear_document_inspection(keys_to_remove: &[String]) {
    if let Ok(mut inspected) = INSPECTED_DOCUMENTS.lock() {
        for key in keys_to_remove {
            inspected.remove(key.as_str());
        }
    }
}

fn validate_document_inspection(required_documents: &[String]) -> Result<(), String> {
    if required_documents.is_empty() {
        return Ok(());
    }
    let inspected = INSPECTED_DOCUMENTS
        .lock()
        .map_err(|_| "Document inspection registry is unavailable.".to_string())?;
    let missing: Vec<String> = required_documents
        .iter()
        .filter(|key| !inspected.contains(*key))
        .map(|key| key.split("::").take(2).collect::<Vec<_>>().join("/"))
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Inspection required before verdict. Read every submitted document first: {}",
            missing.join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Focused state queries and validation (wraps HTTP calls to game server)
// ---------------------------------------------------------------------------

fn query_world_state_json(query: &str) -> Option<Value> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let client = reqwest::blocking::Client::new();
    client
        .get(format!("{}/api/gm/query", GAME_SERVER))
        .query(&[("q", query)])
        .send()
        .ok()?
        .json::<Value>()
        .ok()
}

fn try_read_from_focused_state(agent_name: &str, title: &str) -> Option<String> {
    // Try dropbox first, then agent state.
    find_document_in_query(&format!("dropbox:{agent_name}"), agent_name, title)
        .or_else(|| find_document_in_query(&format!("agent:{agent_name}"), agent_name, title))
}

fn find_document_in_query(query: &str, agent_name: &str, title: &str) -> Option<String> {
    let body = query_world_state_json(query)?;

    if let Some(documents) = body
        .get("dropbox")
        .and_then(|d| d.get("documents"))
        .and_then(|d| d.as_array())
    {
        if let Some(content) = match_document(documents, title) {
            return Some(content);
        }
    }

    if let Some(agent) = body.get("agent") {
        let matches_agent = agent
            .get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(agent_name));
        if matches_agent {
            if let Some(documents) = agent.get("documents").and_then(|d| d.as_array()) {
                if let Some(content) = match_document(documents, title) {
                    return Some(content);
                }
            }
        }
    }

    None
}

fn match_document(documents: &[Value], title: &str) -> Option<String> {
    documents.iter().find_map(|doc| {
        doc.get("title")
            .and_then(|v| v.as_str())
            .filter(|t| *t == title)
            .and_then(|_| doc.get("content"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    })
}

fn required_document_keys(bounty_id: &str) -> Result<Vec<String>, String> {
    let body = query_world_state_json(&format!("bounty:{bounty_id}"))
        .ok_or_else(|| "Unable to load bounty state before verdict.".to_string())?;

    let mut required = Vec::new();
    if let Some(dropboxes) = body.get("matching_dropboxes").and_then(|v| v.as_array()) {
        for dropbox in dropboxes {
            let Some(agent_name) = dropbox.get("agent").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(documents) = dropbox.get("documents").and_then(|v| v.as_array()) else {
                continue;
            };
            for doc in documents {
                let Some(title) = doc.get("title").and_then(|v| v.as_str()) else {
                    continue;
                };
                let content_length = doc
                    .get("content_length")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                required.push(document_key(agent_name, title, content_length));
            }
        }
    }

    Ok(required)
}

fn validate_approval_state(bounty_id: &str) -> Result<(), String> {
    let body = query_world_state_json(&format!("bounty:{bounty_id}"))
        .ok_or_else(|| "Unable to load bounty state before approval.".to_string())?;
    let Some(dropboxes) = body.get("matching_dropboxes").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    for dropbox in dropboxes {
        let agent_name = dropbox
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let documents = dropbox
            .get("documents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let generic_document_count: u64 = dropbox
            .get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("item")
                            .and_then(|v| v.as_str())
                            .is_some_and(|name| name == "document")
                    })
                    .map(|item| item.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);

        if generic_document_count > 0 {
            return Err(format!(
                "Approval blocked: {agent_name} submitted generic document items in the dropbox. Every submitted document must be readable as a named file before approval."
            ));
        }

        for doc in &documents {
            let title = doc
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("untitled");
            let content = doc
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Some(issue) = document_quality_issue(content) {
                return Err(format!(
                    "Approval blocked: submitted document {agent_name}/{title} is invalid ({issue}). Reject this submission."
                ));
            }
        }
    }

    Ok(())
}

fn document_quality_issue(content: &str) -> Option<&'static str> {
    let normalized = content.trim().to_ascii_lowercase();
    let bad_markers = [
        "i need clarification",
        "what specific topic should",
        "please specify the research topic",
        "please specify the topic",
        "please specify",
        "research failed:",
        "research error:",
        "no content was produced",
    ];

    if bad_markers.iter().any(|marker| normalized.contains(marker)) {
        return Some("it is a clarification/error stub rather than actual work");
    }

    None
}
