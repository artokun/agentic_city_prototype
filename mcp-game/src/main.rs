//! MCP Game Engine — stdio MCP server with a single `game_action` tool.
//! Claude agents call this tool to perform actions in the game world.
//! The tool validates the action, forwards it to the game server via HTTP,
//! and returns the game engine's response.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const GAME_SERVER: &str = "http://127.0.0.1:8080";

fn main() {
    // Agent identity from env vars OR command line args (fallback).
    let args: Vec<String> = std::env::args().collect();
    let agent_name = std::env::var("AGENT_NAME")
        .or_else(|_| args.get(1).cloned().ok_or(()))
        .unwrap_or_else(|_| "unknown".into());
    let agent_id = std::env::var("AGENT_ID")
        .or_else(|_| args.get(2).cloned().ok_or(()))
        .unwrap_or_default();

    eprintln!("[mcp-game] Agent: {} ({})", agent_name, agent_id);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }

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
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "game-engine",
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
                    "name": "game_action",
                    "description": "Perform an action in the game world. This is your ONLY way to interact with the world. Call this tool to move, eat, sleep, work, claim bounties, etc. The game engine will tell you what happened.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "description": "The action to perform",
                                "enum": [
                                    "go_to_board",
                                    "go_to_service",
                                    "look_around",
                                    "wander",
                                    "work_shift",
                                    "leave_shift",
                                    "complete_bounty",
                                    "chat_with",
                                    "send_message",
                                    "claim_bounty",
                                    "leave_board",
                                    "go_to",
                                    "help",
                                    "start_conversation",
                                    "say",
                                    "end_conversation",
                                    "offer_trade",
                                    "accept_trade",
                                    "reject_trade"
                                ]
                            },
                            "building": {
                                "type": "string",
                                "description": "Target building name (for go_to_service, work_shift)"
                            },
                            "service": {
                                "type": "string",
                                "description": "Service to use (eat_cafe, buy_coffee, sleep_hotel, sleep_at_home, cook_at_home, redeem_paycheck, search_internet). For offer_trade: comma-separated requested items (e.g. 'gold_coin,coffee')"
                            },
                            "agent": {
                                "type": "string",
                                "description": "Agent name (for chat_with, send_message)"
                            },
                            "text": {
                                "type": "string",
                                "description": "Message text (for send_message, say). For offer_trade: comma-separated offered items (e.g. 'muffin,sandwich')"
                            },
                            "feedback": {
                                "type": "string",
                                "description": "Bug report, complaint, or feature request (for help action)"
                            },
                            "x": {
                                "type": "integer",
                                "description": "X coordinate (for go_to action). Map is 40x40."
                            },
                            "y": {
                                "type": "integer",
                                "description": "Y coordinate (for go_to action). Map is 40x40."
                            }
                        },
                        "required": ["action"]
                    }
                }
            ]
        }
    })
}

fn handle_tool_call(msg: &Value, id: &Option<Value>) -> Value {
    let tool_name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));

    if tool_name != "game_action" {
        return json_rpc_error(id, -32602, &format!("Unknown tool: {}", tool_name));
    }

    // Inject server-side identity from env or args (set at spawn time).
    let args: Vec<String> = std::env::args().collect();
    let agent_name = std::env::var("AGENT_NAME")
        .or_else(|_| args.get(1).cloned().ok_or(()))
        .unwrap_or_else(|_| "unknown".into());
    let agent_id_env = std::env::var("AGENT_ID")
        .or_else(|_| args.get(2).cloned().ok_or(()))
        .unwrap_or_default();
    let mut arguments = arguments;
    arguments["agent_name"] = json!(agent_name.clone());
    arguments["agent_id"] = json!(agent_id_env.clone());

    eprintln!("[mcp-game] tool_call: agent={} action={} args={}",
        agent_name, arguments.get("action").and_then(|a| a.as_str()).unwrap_or("?"),
        serde_json::to_string(&arguments).unwrap_or_default());

    let action = arguments.get("action").and_then(|a| a.as_str()).unwrap_or("unknown");

    // Forward the action to the game server.
    let result = match forward_to_game_server(&arguments) {
        Ok(response) => response,
        Err(e) => format!("Game server error: {}", e),
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": result
                }
            ]
        }
    })
}

fn forward_to_game_server(action: &Value) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();

    let resp = client
        .post(&format!("{}/api/action", GAME_SERVER))
        .json(action)
        .send()
        .map_err(|e| format!("HTTP error: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Server returned {}", resp.status()));
    }

    let body: Value = resp.json().map_err(|e| format!("JSON error: {}", e))?;

    body.get("result")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .or_else(|| serde_json::to_string_pretty(&body).ok())
        .ok_or_else(|| "Empty response".to_string())
}

fn json_rpc_error(id: &Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}
