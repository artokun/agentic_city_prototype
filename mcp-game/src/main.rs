//! MCP Game Engine — stdio MCP server with a single `game_action` tool.
//! Claude agents call this tool to perform actions in the game world.
//! The tool validates the action, forwards it to the game server via HTTP,
//! and returns the game engine's response.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const GAME_SERVER: &str = "http://127.0.0.1:8080";

/// A single game action definition.
struct ActionDef {
    name: &'static str,
    description: &'static str,
    params: &'static str, // brief param hint
}

/// Canonical list of all game actions — used for MCP schema AND manual generation.
fn action_catalog() -> Vec<ActionDef> {
    vec![
        ActionDef { name: "go_to_board", description: "Walk to the bounty board", params: "" },
        ActionDef { name: "go_to_service", description: "Walk to a building and use a service", params: "building=name, service=name" },
        ActionDef { name: "go_to", description: "Walk to specific coordinates", params: "x=int, y=int" },
        ActionDef { name: "look_around", description: "Scan nearby entities and discover buildings", params: "" },
        ActionDef { name: "wander", description: "Walk to a random nearby spot", params: "" },
        ActionDef { name: "work_shift", description: "Start a paid shift at a building", params: "building=name" },
        ActionDef { name: "leave_shift", description: "End your current shift", params: "" },
        ActionDef { name: "claim_bounty", description: "Claim a bounty at the board", params: "text=bounty ID (6-char hex)" },
        ActionDef { name: "complete_bounty", description: "Submit bounty for GM review (must be at board)", params: "" },
        ActionDef { name: "cancel_bounty", description: "Abandon your current bounty (must be at board)", params: "" },
        ActionDef { name: "deposit_item", description: "Put an item into a building's inventory", params: "service=item name" },
        ActionDef { name: "take_item", description: "Take an item from a building's inventory", params: "service=item name" },
        ActionDef { name: "consume_item", description: "Eat/drink an item from your inventory", params: "service=item name (coffee, muffin, rations, sandwich, soup)" },
        ActionDef { name: "inspect_item", description: "Examine an item for details", params: "service=item name" },
        ActionDef { name: "create_document", description: "Write a new document", params: "service=title, text=markdown content" },
        ActionDef { name: "append_document", description: "Add an addendum to an existing document", params: "service=doc title, text=addendum" },
        ActionDef { name: "start_conversation", description: "Begin face-to-face chat with nearby agent", params: "agent=name" },
        ActionDef { name: "say", description: "Speak in your current conversation", params: "text=message" },
        ActionDef { name: "end_conversation", description: "End your current conversation", params: "" },
        ActionDef { name: "send_message", description: "Send async message to a contact (need business card)", params: "agent=name, text=message" },
        ActionDef { name: "offer_trade", description: "Propose a trade in conversation", params: "text=offered items (comma-sep), service=wanted items (comma-sep)" },
        ActionDef { name: "accept_trade", description: "Accept a pending trade offer", params: "" },
        ActionDef { name: "reject_trade", description: "Reject a pending trade offer", params: "" },
        ActionDef { name: "search_library", description: "Search library documents by keyword", params: "service=keyword (empty for full catalog)" },
        ActionDef { name: "copy_document", description: "Copy a library document to your inventory", params: "service=document title" },
        ActionDef { name: "leave_board", description: "Leave the bounty board area", params: "" },
        ActionDef { name: "chat_with", description: "Quick chat with nearby agent (legacy)", params: "agent=name" },
        ActionDef { name: "help", description: "Submit feedback/bug report to developers", params: "text=your feedback" },
    ]
}

/// Generate a game manual from the action catalog.
fn generate_manual() -> String {
    let actions = action_catalog();
    let mut manual = String::from("## Actions (use game_action MCP tool)\n\n");

    for a in &actions {
        if a.params.is_empty() {
            manual += &format!("- **{}** — {}\n", a.name, a.description);
        } else {
            manual += &format!("- **{}** — {} ({})\n", a.name, a.description, a.params);
        }
    }

    manual += "\n## Consumable Items (use consume_item)\n";
    manual += "| Item | Effect |\n";
    manual += "|------|--------|\n";
    manual += "| coffee | +10k context token ceiling (think longer before sleeping) |\n";
    manual += "| muffin | +30 hunger |\n";
    manual += "| rations | +50 hunger |\n";
    manual += "| sandwich | +60 hunger, +10 boredom |\n";
    manual += "| soup | +45 hunger |\n";
    manual += "\n## Tips\n";
    manual += "- Bounties pay 5-20g — much faster than shifts\n";
    manual += "- Coffee extends thinking capacity — save it for when you need it\n";
    manual += "- Trading requires a face-to-face conversation first\n";
    manual += "- bounty_token and paycheck CANNOT be traded\n";
    manual += "- Use help action to report bugs or request features\n";

    manual
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // --generate-manual: print manual to stdout and exit.
    if args.iter().any(|a| a == "--generate-manual") {
        print!("{}", generate_manual());
        return;
    }

    // --list-actions: print JSON array of action names and exit.
    if args.iter().any(|a| a == "--list-actions") {
        let names: Vec<&str> = action_catalog().iter().map(|a| a.name).collect();
        println!("{}", serde_json::to_string(&names).unwrap());
        return;
    }

    // Agent identity from env vars OR command line args (fallback).
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
    let actions = action_catalog();
    let action_names: Vec<Value> = actions.iter().map(|a| json!(a.name)).collect();
    let action_descriptions: Vec<String> = actions
        .iter()
        .map(|a| {
            if a.params.is_empty() {
                a.description.to_string()
            } else {
                format!("{} ({})", a.description, a.params)
            }
        })
        .collect();
    let action_desc_str = action_descriptions.join("; ");

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "game_action",
                    "description": format!("Perform an action in the game world. This is your ONLY way to interact. Actions: {}", action_desc_str),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "description": "The action to perform",
                                "enum": action_names
                            },
                            "building": {
                                "type": "string",
                                "description": "Target building name (for go_to_service, work_shift)"
                            },
                            "service": {
                                "type": "string",
                                "description": "Service name, item name, or document title — depends on the action. For deposit_item/take_item/consume_item: the item name. For create_document/append_document: the document title. For offer_trade: comma-separated requested items. For search_library: keyword."
                            },
                            "agent": {
                                "type": "string",
                                "description": "Agent name (for start_conversation, chat_with, send_message)"
                            },
                            "text": {
                                "type": "string",
                                "description": "Message text (for say, send_message). For claim_bounty: bounty ID. For create_document: markdown content. For append_document: addendum. For offer_trade: comma-separated offered items."
                            },
                            "feedback": {
                                "type": "string",
                                "description": "Bug report or feature request (for help action)"
                            },
                            "x": {
                                "type": "integer",
                                "description": "X coordinate (for go_to). Map is 40x200."
                            },
                            "y": {
                                "type": "integer",
                                "description": "Y coordinate (for go_to). Map is 40x200."
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
    let tool_name = msg
        .pointer("/params/name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let arguments = msg
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(json!({}));

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

    eprintln!(
        "[mcp-game] tool_call: agent={} action={} args={}",
        agent_name,
        arguments
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("?"),
        serde_json::to_string(&arguments).unwrap_or_default()
    );

    let action = arguments
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("unknown");

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
