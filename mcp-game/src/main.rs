//! MCP Game Engine — thin stdio MCP wrapper over the shared tool runtime.
//! Claude agents call `game_action` to perform actions in the game world.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

use server::llm::tools::catalog::{self, tools_for_set};
use server::llm::tools::execute::execute_game_action;
use server::llm::tools::schema::to_mcp_tools_list;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // --generate-manual: print manual to stdout and exit.
    if args.iter().any(|a| a == "--generate-manual") {
        print!("{}", catalog::generate_action_manual());
        return;
    }

    // --list-actions: print JSON array of action names and exit.
    if args.iter().any(|a| a == "--list-actions") {
        let names: Vec<&str> = catalog::game_action_catalog()
            .iter()
            .map(|a| a.name)
            .collect();
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
            "tools/call" => handle_tool_call(&msg, &id, &agent_name, &agent_id),
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
            "serverInfo": { "name": "game-engine", "version": "0.1.0" }
        }
    })
}

fn handle_tools_list(id: &Option<Value>) -> Value {
    let tools = tools_for_set("game");
    let result = to_mcp_tools_list(&tools);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn handle_tool_call(
    msg: &Value,
    id: &Option<Value>,
    agent_name: &str,
    agent_id: &str,
) -> Value {
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

    eprintln!(
        "[mcp-game] tool_call: agent={} action={} args={}",
        agent_name,
        arguments
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("?"),
        serde_json::to_string(&arguments).unwrap_or_default()
    );

    let result = execute_game_action(&arguments, agent_name, agent_id);

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": result.output }],
            "isError": result.is_error,
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
