//! Canonical tool and action definitions for game-agent and system-ai tool sets.
//! Single source of truth — both MCP binaries and in-process adapters use these.

use serde::Serialize;
use serde_json::{json, Value};
use std::sync::LazyLock;

/// A parameter on a tool.
#[derive(Debug, Clone, Serialize)]
pub struct ParamDef {
    pub name: &'static str,
    pub description: &'static str,
    pub param_type: ParamType,
    pub required: bool,
}

/// Supported parameter types.
#[derive(Debug, Clone, Serialize)]
pub enum ParamType {
    String,
    Integer,
    /// String with a fixed set of allowed values.
    Enum(Vec<&'static str>),
}

/// A single tool definition with its parameters.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<ParamDef>,
    /// Which tool set this belongs to.
    pub tool_set: &'static str,
}

/// A game action within the `game_action` tool.
#[derive(Debug, Clone)]
pub struct ActionDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static str,
}

// ---------------------------------------------------------------------------
// Game action catalog (drives the game_action tool)
// ---------------------------------------------------------------------------

/// Canonical list of all game actions. Source of truth for MCP schema AND manual generation.
pub fn game_action_catalog() -> Vec<ActionDef> {
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

/// Generate the game manual text from the action catalog.
pub fn generate_action_manual() -> String {
    let actions = game_action_catalog();
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

// ---------------------------------------------------------------------------
// Tool definitions by tool set
// ---------------------------------------------------------------------------

/// Returns all tool definitions for a given tool set name.
pub fn tools_for_set(tool_set: &str) -> Vec<ToolDef> {
    match tool_set {
        "game" => game_tools(),
        "system" => system_tools(),
        _ => vec![],
    }
}

/// Lazily computed game_action description string (avoids Box::leak).
static GAME_ACTION_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    let actions = game_action_catalog();
    let action_desc: String = actions
        .iter()
        .map(|a| {
            if a.params.is_empty() {
                a.description.to_string()
            } else {
                format!("{} ({})", a.description, a.params)
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Perform an action in the game world. This is your ONLY way to interact. Actions: {}",
        action_desc
    )
});

/// Game-agent tool set: a single `game_action` tool with action enum.
fn game_tools() -> Vec<ToolDef> {
    let actions = game_action_catalog();
    let action_names: Vec<&'static str> = actions.iter().map(|a| a.name).collect();

    vec![ToolDef {
        name: "game_action",
        // LazyLock is 'static, so .as_str() yields &'static str.
        description: GAME_ACTION_DESCRIPTION.as_str(),
        params: vec![
            ParamDef {
                name: "action",
                description: "The action to perform",
                param_type: ParamType::Enum(action_names),
                required: true,
            },
            ParamDef {
                name: "building",
                description: "Target building name (for go_to_service, work_shift)",
                param_type: ParamType::String,
                required: false,
            },
            ParamDef {
                name: "service",
                description: "Service name, item name, or document title — depends on the action. For deposit_item/take_item/consume_item: the item name. For create_document/append_document: the document title. For offer_trade: comma-separated requested items. For search_library: keyword.",
                param_type: ParamType::String,
                required: false,
            },
            ParamDef {
                name: "agent",
                description: "Agent name (for start_conversation, chat_with, send_message)",
                param_type: ParamType::String,
                required: false,
            },
            ParamDef {
                name: "text",
                description: "Message text (for say, send_message). For claim_bounty: bounty ID. For create_document: markdown content. For append_document: addendum. For offer_trade: comma-separated offered items.",
                param_type: ParamType::String,
                required: false,
            },
            ParamDef {
                name: "feedback",
                description: "Bug report or feature request (for help action)",
                param_type: ParamType::String,
                required: false,
            },
            ParamDef {
                name: "x",
                description: "X coordinate (for go_to). Map is 40x200.",
                param_type: ParamType::Integer,
                required: false,
            },
            ParamDef {
                name: "y",
                description: "Y coordinate (for go_to). Map is 40x200.",
                param_type: ParamType::Integer,
                required: false,
            },
        ],
        tool_set: "game",
    }]
}

/// System-AI tool set: query, read_document, approve, reject, grant_gold.
fn system_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "query_world_state",
            description: "Query focused world state only. Use agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>. Full dumps are not allowed.",
            params: vec![ParamDef {
                name: "query",
                description: "Focused sub-query only: agent:<name>, bounty:<id>, dropbox:<agent>, or structure:<name>. Do not request full world dumps.",
                param_type: ParamType::String,
                required: true,
            }],
            tool_set: "system",
        },
        ToolDef {
            name: "read_document",
            description: "Read the full contents of a named document. This is mandatory before resolving any submission that includes documents.",
            params: vec![
                ParamDef {
                    name: "agent_name",
                    description: "Agent owner of the document",
                    param_type: ParamType::String,
                    required: true,
                },
                ParamDef {
                    name: "title",
                    description: "Exact document title, such as research_123abc.md",
                    param_type: ParamType::String,
                    required: true,
                },
            ],
            tool_set: "system",
        },
        ToolDef {
            name: "approve",
            description: "Approve a bounty after reviewing focused world state.",
            params: vec![
                ParamDef {
                    name: "bounty_id",
                    description: "The bounty UUID being verified",
                    param_type: ParamType::String,
                    required: true,
                },
                ParamDef {
                    name: "message",
                    description: "Short in-character approval message for viewers",
                    param_type: ParamType::String,
                    required: true,
                },
            ],
            tool_set: "system",
        },
        ToolDef {
            name: "grant_gold",
            description: "Grant a specific amount of gold to an agent for a specific reason. Use this sparingly and only when the bounty rules explicitly justify it.",
            params: vec![
                ParamDef {
                    name: "agent_name",
                    description: "Agent to reward",
                    param_type: ParamType::String,
                    required: true,
                },
                ParamDef {
                    name: "amount",
                    description: "Exact gold amount to grant",
                    param_type: ParamType::Integer,
                    required: true,
                },
                ParamDef {
                    name: "reason",
                    description: "Internal reason for the grant",
                    param_type: ParamType::String,
                    required: true,
                },
                ParamDef {
                    name: "message",
                    description: "Short viewer-facing message explaining the award",
                    param_type: ParamType::String,
                    required: true,
                },
            ],
            tool_set: "system",
        },
        ToolDef {
            name: "reject",
            description: "Reject a bounty after reviewing focused world state.",
            params: vec![
                ParamDef {
                    name: "bounty_id",
                    description: "The bounty UUID being verified",
                    param_type: ParamType::String,
                    required: true,
                },
                ParamDef {
                    name: "message",
                    description: "Short in-character rejection message for viewers",
                    param_type: ParamType::String,
                    required: true,
                },
            ],
            tool_set: "system",
        },
    ]
}
