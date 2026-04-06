//! MCP System AI server for bounty verification.
//! Used by the persistent Claude System AI session to inspect focused
//! world-state slices and approve or reject bounty submissions.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::{LazyLock, Mutex};

const GAME_SERVER: &str = "http://127.0.0.1:8080";
const ALLOWED_QUERY_PREFIXES: [&str; 4] = ["agent:", "bounty:", "dropbox:", "structure:"];
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
            "serverInfo": {
                "name": "system-ai",
                "version": "0.1.0"
            }
        }
    })
}

fn handle_tools_list(id: &Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "query_world_state",
                    "description": "Query focused world state only. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>. Full dumps are not allowed.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Focused sub-query only: agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>. Do not request full world dumps."
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "read_document",
                    "description": "Read the full contents of a named document. This is mandatory before resolving any submission that includes documents.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "agent_name": {
                                "type": "string",
                                "description": "Agent owner of the document"
                            },
                            "title": {
                                "type": "string",
                                "description": "Exact document title, such as research_123abc.md"
                            }
                        },
                        "required": ["agent_name", "title"]
                    }
                },
                {
                    "name": "approve",
                    "description": "Approve a bounty after reviewing focused world state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "bounty_id": {
                                "type": "string",
                                "description": "The bounty UUID being verified"
                            },
                            "message": {
                                "type": "string",
                                "description": "Short in-character approval message for viewers"
                            }
                        },
                        "required": ["bounty_id", "message"]
                    }
                },
                {
                    "name": "grant_gold",
                    "description": "Grant a specific amount of gold to an agent for a specific reason. Use this sparingly and only when the bounty rules explicitly justify it.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "agent_name": {
                                "type": "string",
                                "description": "Agent to reward"
                            },
                            "amount": {
                                "type": "integer",
                                "description": "Exact gold amount to grant"
                            },
                            "reason": {
                                "type": "string",
                                "description": "Internal reason for the grant"
                            },
                            "message": {
                                "type": "string",
                                "description": "Short viewer-facing message explaining the award"
                            }
                        },
                        "required": ["agent_name", "amount", "reason", "message"]
                    }
                },
                {
                    "name": "reject",
                    "description": "Reject a bounty after reviewing focused world state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "bounty_id": {
                                "type": "string",
                                "description": "The bounty UUID being verified"
                            },
                            "message": {
                                "type": "string",
                                "description": "Short in-character rejection message for viewers"
                            }
                        },
                        "required": ["bounty_id", "message"]
                    }
                }
            ]
        }
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
        "query_world_state" => {
            let query = arguments
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("");
            query_world_state(query)
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
            read_document(agent_name, title)
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
            submit_verdict(bounty_id, true, message)
        }
        "grant_gold" => {
            let agent_name = arguments
                .get("agent_name")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let amount = arguments
                .get("amount")
                .and_then(|a| a.as_u64())
                .unwrap_or(0) as u32;
            let reason = arguments
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let message = arguments
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            grant_gold(agent_name, amount, reason, message)
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
            submit_verdict(bounty_id, false, message)
        }
        _ => format!("Unknown tool: {}", tool_name),
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": result }]
        }
    })
}

fn query_world_state(query: &str) -> String {
    let query = query.trim();
    if query == "full" || query.is_empty() {
        return "Focused queries only. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>.".into();
    }
    if !ALLOWED_QUERY_PREFIXES
        .iter()
        .any(|prefix| query.starts_with(prefix))
    {
        return "Unsupported query. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>.".into();
    }

    let client = reqwest::blocking::Client::new();

    match client
        .get(format!("{}/api/gm/query", GAME_SERVER))
        .query(&[("q", query)])
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                resp.text().unwrap_or_else(|_| "Empty response".into())
            } else {
                format!("Server error: {}", resp.status())
            }
        }
        Err(e) => format!("Connection error: {}", e),
    }
}

fn read_document(agent_name: &str, title: &str) -> String {
    let agent_name = agent_name.trim().to_ascii_lowercase();
    let title = title.trim();
    if agent_name.is_empty() || title.is_empty() {
        return "read_document requires non-empty agent_name and title.".into();
    }

    if let Some(content) = read_document_from_focused_state(&agent_name, title) {
        return content;
    }

    let mut url = match reqwest::Url::parse(GAME_SERVER) {
        Ok(url) => url,
        Err(err) => return format!("Invalid game server URL: {}", err),
    };

    if url
        .path_segments_mut()
        .map(|mut segments| {
            segments.extend(["api", "documents", agent_name.as_str(), title]);
        })
        .is_err()
    {
        return "Failed to build document URL.".into();
    }

    let client = reqwest::blocking::Client::new();
    match client.get(url).send() {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.text() {
                    Ok(content) => {
                        mark_document_inspected(&agent_name, title, content.len());
                        content
                    }
                    Err(_) => "Empty document".into(),
                }
            } else {
                format!("Server error: {}", resp.status())
            }
        }
        Err(err) => format!("Connection error: {}", err),
    }
}

fn submit_verdict(bounty_id: &str, approved: bool, reason: &str) -> String {
    if bounty_id.trim().is_empty() {
        return "approve/reject requires a non-empty bounty_id.".into();
    }
    if approved {
        if let Err(err) = validate_approval_state(bounty_id) {
            return err;
        }
    }
    let keys_to_clear = match required_document_keys(bounty_id) {
        Ok(keys) => keys,
        Err(err) => return err,
    };
    if let Err(err) = validate_document_inspection(&keys_to_clear) {
        return err;
    }

    let client = reqwest::blocking::Client::new();

    let body = json!({
        "bounty_id": bounty_id,
        "approved": approved,
        "reason": reason,
    });

    match client
        .post(&format!("{}/api/gm/verdict", GAME_SERVER))
        .json(&body)
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                clear_document_inspection(&keys_to_clear);
                resp.text().unwrap_or_else(|_| "Verdict submitted".into())
            } else {
                format!("Server error: {}", resp.status())
            }
        }
        Err(e) => format!("Connection error: {}", e),
    }
}

fn grant_gold(agent_name: &str, amount: u32, reason: &str, message: &str) -> String {
    if agent_name.trim().is_empty() {
        return "grant_gold requires a non-empty agent_name.".into();
    }
    if amount == 0 {
        return "grant_gold requires amount > 0.".into();
    }
    if reason.trim().is_empty() {
        return "grant_gold requires a non-empty reason.".into();
    }
    if message.trim().is_empty() {
        return "grant_gold requires a viewer-facing message.".into();
    }

    let client = reqwest::blocking::Client::new();
    let body = json!({
        "agent_name": agent_name,
        "amount": amount,
        "reason": reason,
        "message": message,
    });

    match client
        .post(&format!("{}/api/gm/grant_gold", GAME_SERVER))
        .json(&body)
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                resp.text().unwrap_or_else(|_| "Gold grant submitted".into())
            } else {
                format!("Server error: {}", resp.status())
            }
        }
        Err(e) => format!("Connection error: {}", e),
    }
}

fn read_document_from_focused_state(agent_name: &str, title: &str) -> Option<String> {
    if let Some(content) = find_document_in_query(&format!("dropbox:{agent_name}"), agent_name, title)
    {
        mark_document_inspected(agent_name, title, content.len());
        return Some(content);
    }

    if let Some(content) = find_document_in_query(&format!("agent:{agent_name}"), agent_name, title)
    {
        mark_document_inspected(agent_name, title, content.len());
        return Some(content);
    }

    None
}

fn find_document_in_query(query: &str, agent_name: &str, title: &str) -> Option<String> {
    let body = query_world_state_json(query)?;

    if let Some(documents) = body
        .get("dropbox")
        .and_then(|dropbox| dropbox.get("documents"))
        .and_then(|documents| documents.as_array())
    {
        if let Some(content) = match_document(documents, title) {
            return Some(content);
        }
    }

    if let Some(agent) = body.get("agent") {
        let matches_agent = agent
            .get("name")
            .and_then(|name| name.as_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(agent_name));
        if matches_agent {
            if let Some(documents) = agent.get("documents").and_then(|documents| documents.as_array())
            {
                if let Some(content) = match_document(documents, title) {
                    return Some(content);
                }
            }
        }
    }

    None
}

fn match_document(documents: &[Value], title: &str) -> Option<String> {
    documents.iter().find_map(|document| {
        document
            .get("title")
            .and_then(|value| value.as_str())
            .filter(|document_title| *document_title == title)
            .and_then(|_| document.get("content"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    })
}

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

fn required_document_keys(bounty_id: &str) -> Result<Vec<String>, String> {
    let body = query_world_state_json(&format!("bounty:{bounty_id}"))
        .ok_or_else(|| "Unable to load bounty state before verdict.".to_string())?;

    let mut required_documents = Vec::new();
    if let Some(dropboxes) = body.get("matching_dropboxes").and_then(|value| value.as_array()) {
        for dropbox in dropboxes {
            let Some(agent_name) = dropbox.get("agent").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(documents) = dropbox.get("documents").and_then(|value| value.as_array()) else {
                continue;
            };

            for document in documents {
                let Some(title) = document.get("title").and_then(|value| value.as_str()) else {
                    continue;
                };
                let content_length = document
                    .get("content_length")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                required_documents.push(document_key(agent_name, title, content_length));
            }
        }
    }

    Ok(required_documents)
}

fn validate_approval_state(bounty_id: &str) -> Result<(), String> {
    let body = query_world_state_json(&format!("bounty:{bounty_id}"))
        .ok_or_else(|| "Unable to load bounty state before approval.".to_string())?;
    let Some(dropboxes) = body.get("matching_dropboxes").and_then(|value| value.as_array()) else {
        return Ok(());
    };

    for dropbox in dropboxes {
        let agent_name = dropbox
            .get("agent")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let documents = dropbox
            .get("documents")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let generic_document_count: u64 = dropbox
            .get("items")
            .and_then(|value| value.as_array())
            .map(|items| {
                items.iter()
                    .filter(|item| {
                        item.get("item")
                            .and_then(|value| value.as_str())
                            .is_some_and(|name| name == "document")
                    })
                    .map(|item| {
                        item.get("count")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0)
                    })
                    .sum()
            })
            .unwrap_or(0);

        if generic_document_count > 0 {
            return Err(format!(
                "Approval blocked: {agent_name} submitted generic document items in the dropbox. Every submitted document must be readable as a named file before approval."
            ));
        }

        for document in &documents {
            let title = document
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("untitled");
            let content = document
                .get("content")
                .and_then(|value| value.as_str())
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

fn clear_document_inspection(keys_to_remove: &[String]) {
    if let Ok(mut inspected) = INSPECTED_DOCUMENTS.lock() {
        for key in keys_to_remove {
            inspected.remove(key.as_str());
        }
    }
}

fn json_rpc_error(id: &Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
