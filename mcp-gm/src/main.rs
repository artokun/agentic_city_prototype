//! MCP Game Master — stdio MCP server for bounty verification.
//! Spawned as a one-shot process by the game server when an agent
//! calls complete_bounty. The GM queries world state, evaluates the
//! bounty's hidden acceptance criteria, and submits a verdict.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const GAME_SERVER: &str = "http://127.0.0.1:8080";

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }

        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };

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
                "name": "game-master",
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
                    "description": "Query the current world state. Returns all agent positions, inventories, goals, needs, and structure inventories. Use this to verify bounty completion criteria.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "What to query: 'full' for everything, 'agent:<name>' for specific agent, 'structure:<name>' for specific building, 'bounties' for all bounties, 'logs:<agent_name>' for agent's action log"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "submit_verdict",
                    "description": "Submit your verdict on the bounty completion. Call this ONCE after reviewing the world state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "approved": {
                                "type": "boolean",
                                "description": "true if the bounty was completed correctly, false to reject"
                            },
                            "reason": {
                                "type": "string",
                                "description": "Brief explanation of why you approved or rejected"
                            },
                            "bounty_id": {
                                "type": "string",
                                "description": "The bounty UUID being verified"
                            }
                        },
                        "required": ["approved", "reason", "bounty_id"]
                    }
                }
            ]
        }
    })
}

fn handle_tool_call(msg: &Value, id: &Option<Value>) -> Value {
    let tool_name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "query_world_state" => {
            let query = arguments.get("query").and_then(|q| q.as_str()).unwrap_or("full");
            query_world_state(query)
        }
        "submit_verdict" => {
            let approved = arguments.get("approved").and_then(|a| a.as_bool()).unwrap_or(false);
            let reason = arguments.get("reason").and_then(|r| r.as_str()).unwrap_or("no reason");
            let bounty_id = arguments.get("bounty_id").and_then(|b| b.as_str()).unwrap_or("");
            submit_verdict(bounty_id, approved, reason)
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
    let client = reqwest::blocking::Client::new();

    let url = format!("{}/api/gm/query?q={}", GAME_SERVER, query);
    match client.get(&url).send() {
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

fn submit_verdict(bounty_id: &str, approved: bool, reason: &str) -> String {
    let client = reqwest::blocking::Client::new();

    let body = json!({
        "bounty_id": bounty_id,
        "approved": approved,
        "reason": reason,
    });

    match client.post(&format!("{}/api/gm/verdict", GAME_SERVER))
        .json(&body)
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                resp.text().unwrap_or_else(|_| "Verdict submitted".into())
            } else {
                format!("Server error: {}", resp.status())
            }
        }
        Err(e) => format!("Connection error: {}", e),
    }
}

fn json_rpc_error(id: &Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
