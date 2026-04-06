//! MCP System AI server — thin stdio MCP wrapper over the shared tool runtime.
//! Used by the persistent System AI session to verify bounty submissions.
//!
//! Document inspection tracking is process-local state. All validation logic
//! (approval checks, document quality, etc.) lives in server::llm::tools::policy.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::{LazyLock, Mutex};

use server::llm::tools::catalog::tools_for_set;
use server::llm::tools::execute::execute_system_tool;
use server::llm::tools::policy;
use server::llm::tools::schema::to_mcp_tools_list;

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

    let (result, is_error) = match tool_name {
        "read_document" => {
            let agent_name = arguments
                .get("agent_name")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let title = arguments
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("");

            let (doc_result, is_err) = match policy::try_read_from_focused_state(agent_name, title)
            {
                Some(content) => (content, false),
                None => {
                    let r = execute_system_tool(tool_name, &arguments, None);
                    (r.output, r.is_error)
                }
            };

            if !is_err {
                mark_document_inspected(agent_name, title, doc_result.len());
            }
            (doc_result, is_err)
        }
        "approve" | "reject" => {
            let bounty_id = arguments
                .get("bounty_id")
                .and_then(|b| b.as_str())
                .unwrap_or("");
            let approved = tool_name == "approve";

            if approved {
                if let Err(err) = policy::validate_approval_state(bounty_id) {
                    return tool_result(id, &err, true);
                }
            }
            let keys_to_clear = match policy::required_document_keys(bounty_id) {
                Ok(keys) => keys,
                Err(err) => return tool_result(id, &err, true),
            };
            let inspected = inspected_snapshot();
            if let Err(err) = policy::validate_document_inspection(&keys_to_clear, &inspected) {
                return tool_result(id, &err, true);
            }

            let r = execute_system_tool(tool_name, &arguments, None);
            if !r.is_error {
                clear_document_inspection(&keys_to_clear);
            }
            (r.output, r.is_error)
        }
        "query_world_state" | "grant_gold" => {
            let r = execute_system_tool(tool_name, &arguments, None);
            (r.output, r.is_error)
        }
        _ => (format!("Unknown tool: {}", tool_name), true),
    };

    tool_result(id, &result, is_error)
}

fn tool_result(id: &Option<Value>, text: &str, is_error: bool) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
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
// Document inspection tracking (process-local state only)
// ---------------------------------------------------------------------------

fn mark_document_inspected(agent_name: &str, title: &str, content_length: usize) {
    if let Ok(mut inspected) = INSPECTED_DOCUMENTS.lock() {
        inspected.insert(policy::document_key(agent_name, title, content_length));
    }
}

fn inspected_snapshot() -> HashSet<String> {
    INSPECTED_DOCUMENTS
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

fn clear_document_inspection(keys_to_remove: &[String]) {
    if let Ok(mut inspected) = INSPECTED_DOCUMENTS.lock() {
        for key in keys_to_remove {
            inspected.remove(key.as_str());
        }
    }
}
