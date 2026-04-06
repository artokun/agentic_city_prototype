//! GM policy logic for bounty verdict validation.
//! Extracted from mcp-gm so the binary stays thin and this logic can be
//! reused by in-process adapters in future gates.

use serde_json::Value;

const DEFAULT_GAME_SERVER: &str = "http://127.0.0.1:8080";

fn game_server_url() -> String {
    std::env::var("GAME_SERVER_URL").unwrap_or_else(|_| DEFAULT_GAME_SERVER.to_string())
}

/// Query world state as parsed JSON.
pub fn query_world_state_json(query: &str) -> Option<Value> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let client = reqwest::blocking::Client::new();
    client
        .get(format!("{}/api/gm/query", game_server_url()))
        .query(&[("q", query)])
        .send()
        .ok()?
        .json::<Value>()
        .ok()
}

/// Try to read a document from focused world state queries (dropbox, then agent).
pub fn try_read_from_focused_state(agent_name: &str, title: &str) -> Option<String> {
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

/// Build a document key for inspection tracking.
pub fn document_key(agent_name: &str, title: &str, content_length: usize) -> String {
    format!(
        "{}::{}::{}",
        agent_name.to_ascii_lowercase(),
        title,
        content_length
    )
}

/// Get the list of document keys that must be inspected before a verdict.
pub fn required_document_keys(bounty_id: &str) -> Result<Vec<String>, String> {
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

/// Validate that approval can proceed (no generic documents, no bad content).
pub fn validate_approval_state(bounty_id: &str) -> Result<(), String> {
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

/// Check document content for quality issues (stubs, errors, placeholders).
pub fn document_quality_issue(content: &str) -> Option<&'static str> {
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

/// Validate that all required documents have been inspected.
pub fn validate_document_inspection(
    required_documents: &[String],
    inspected: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if required_documents.is_empty() {
        return Ok(());
    }
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
