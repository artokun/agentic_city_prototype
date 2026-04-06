//! Schema compilation: convert canonical ToolDefs into provider-specific formats.
//! Supports MCP JSON-RPC, OpenAI function-calling, and raw JSON Schema.

use serde_json::{json, Value};

use super::catalog::{ParamType, ToolDef};

/// Compile a tool list into MCP `tools/list` response format.
pub fn to_mcp_tools_list(tools: &[ToolDef]) -> Value {
    let tool_schemas: Vec<Value> = tools.iter().map(|t| to_mcp_tool(t)).collect();
    json!({ "tools": tool_schemas })
}

/// Compile a single tool into MCP tool schema.
fn to_mcp_tool(tool: &ToolDef) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for param in &tool.params {
        properties.insert(param.name.to_string(), param_to_json_schema(param));
        if param.required {
            required.push(json!(param.name));
        }
    }

    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    })
}

/// Compile a tool list into OpenAI function-calling format.
/// Each tool becomes a `{"type": "function", "function": {...}}` entry.
pub fn to_openai_functions(tools: &[ToolDef]) -> Vec<Value> {
    tools.iter().map(|t| to_openai_function(t)).collect()
}

/// Compile a single tool into OpenAI function-calling format.
fn to_openai_function(tool: &ToolDef) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for param in &tool.params {
        properties.insert(param.name.to_string(), param_to_json_schema(param));
        if param.required {
            required.push(json!(param.name));
        }
    }

    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        }
    })
}

/// Compile a tool list into raw JSON Schema (for generic use).
pub fn to_json_schemas(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for param in &t.params {
                properties.insert(param.name.to_string(), param_to_json_schema(param));
                if param.required {
                    required.push(json!(param.name));
                }
            }
            json!({
                "name": t.name,
                "description": t.description,
                "schema": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                }
            })
        })
        .collect()
}

/// Convert a ParamDef into a JSON Schema property object.
fn param_to_json_schema(param: &super::catalog::ParamDef) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert(
        "description".to_string(),
        json!(param.description),
    );

    match &param.param_type {
        ParamType::String => {
            schema.insert("type".to_string(), json!("string"));
        }
        ParamType::Integer => {
            schema.insert("type".to_string(), json!("integer"));
        }
        ParamType::Enum(values) => {
            schema.insert("type".to_string(), json!("string"));
            schema.insert("enum".to_string(), json!(values));
        }
    }

    Value::Object(schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::tools::catalog::tools_for_set;

    #[test]
    fn mcp_game_tools_have_game_action() {
        let tools = tools_for_set("game");
        let mcp = to_mcp_tools_list(&tools);
        let tool_list = mcp["tools"].as_array().unwrap();
        assert_eq!(tool_list.len(), 1);
        assert_eq!(tool_list[0]["name"], "game_action");
        assert!(tool_list[0]["inputSchema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .len()
            > 20);
    }

    #[test]
    fn mcp_system_tools_have_five_tools() {
        let tools = tools_for_set("system");
        let mcp = to_mcp_tools_list(&tools);
        let tool_list = mcp["tools"].as_array().unwrap();
        assert_eq!(tool_list.len(), 5);
        let names: Vec<&str> = tool_list
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"query_world_state"));
        assert!(names.contains(&"approve"));
        assert!(names.contains(&"reject"));
        assert!(names.contains(&"read_document"));
        assert!(names.contains(&"grant_gold"));
    }

    #[test]
    fn openai_functions_format() {
        let tools = tools_for_set("game");
        let funcs = to_openai_functions(&tools);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0]["type"], "function");
        assert_eq!(funcs[0]["function"]["name"], "game_action");
        assert!(funcs[0]["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("action"));
    }

    #[test]
    fn openai_system_functions_format() {
        let tools = tools_for_set("system");
        let funcs = to_openai_functions(&tools);
        assert_eq!(funcs.len(), 5);
        for func in &funcs {
            assert_eq!(func["type"], "function");
            assert!(func["function"]["name"].as_str().is_some());
            assert!(func["function"]["parameters"]["type"] == "object");
        }
    }
}
