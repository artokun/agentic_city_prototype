use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::agents::perception::KnownLocations;
use crate::agents::social::Relationships;
use crate::items::{Inventory, ItemType};
use crate::world::bounty::BountyRegistry;
use crate::world::map::GridPos;

/// Build the decision context for an agent to send to Claude.
pub fn build_context(
    name: &str,
    pos: &GridPos,
    needs: &Needs,
    inv: &Inventory,
    goal: &AgentGoal,
    known_locations: &KnownLocations,
    relationships: &Relationships,
    speed: f32,
    available_bounties: &[String],
    nearby_agents: &[(String, GridPos)],
    location_tools: &[&str],
) -> String {
    let gold = inv.count(ItemType::GoldCoin);

    let mut ctx = format!(
r#"You are {name}, an agent in San Francisco. Make decisions to maximize gold while keeping your needs above critical levels.

## Current State
- Position: ({}, {})
- Gold: {gold}
- Speed: {speed} tiles/sec
- Energy: {:.0}/100  Hunger: {:.0}/100  Boredom: {:.0}/100
- Current goal: {:?}
"#,
        pos.x, pos.y, needs.energy, needs.hunger, needs.boredom, goal,
    );

    // Inventory
    let items: Vec<String> = inv.items.iter()
        .filter(|(t, _)| **t != ItemType::GoldCoin)
        .map(|(t, c)| format!("{} x{}", t, c))
        .collect();
    if !items.is_empty() {
        ctx += &format!("- Inventory: {}\n", items.join(", "));
    }

    // Known locations with distances
    ctx += "\n## Known Locations\n";
    if known_locations.locations.is_empty() {
        ctx += "- Only know the bounty board\n";
    } else {
        let mut locs: Vec<_> = known_locations.locations.values().collect();
        locs.sort_by_key(|l| {
            (l.entrance.x - pos.x).abs() + (l.entrance.y - pos.y).abs()
        });
        for loc in locs.iter().take(15) {
            let dist = (loc.entrance.x - pos.x).abs() + (loc.entrance.y - pos.y).abs();
            let travel_secs = dist as f32 / speed;
            ctx += &format!("- {} at ({},{}) entrance ({},{}) — {} tiles, {:.1}s travel\n",
                loc.name, loc.pos.x, loc.pos.y, loc.entrance.x, loc.entrance.y, dist, travel_secs);
        }
    }

    // Available bounties
    ctx += "\n## Available Bounties\n";
    if available_bounties.is_empty() {
        ctx += "- None currently (check the bounty board)\n";
    } else {
        for b in available_bounties {
            ctx += &format!("- {}\n", b);
        }
    }

    // Nearby agents
    if !nearby_agents.is_empty() {
        ctx += "\n## Nearby Agents\n";
        for (aname, apos) in nearby_agents {
            let dist = (apos.x - pos.x).abs() + (apos.y - pos.y).abs();
            ctx += &format!("- {} at ({},{}) — {} tiles away\n", aname, apos.x, apos.y, dist);
        }
    }

    // Relationships
    if !relationships.known.is_empty() {
        ctx += "\n## Known Agents\n";
        for mem in relationships.known.values() {
            ctx += &format!("- {} (friendship: {}, last seen doing: {})\n",
                mem.name, mem.friendship, mem.last_known_goal);
        }
    }

    // Location-specific tools
    if !location_tools.is_empty() {
        ctx += "\n## Available Tools (at current location)\n";
        for tool in location_tools {
            ctx += &format!("- {}\n", tool);
        }
    }

    // Action format
    ctx += r#"
## Respond with ONE JSON action:
```json
{"action": "go_to_board", "thought": "why"}
{"action": "go_to_service", "building": "cafe", "service": "eat_cafe", "thought": "why"}
{"action": "go_to_service", "building": "hotel", "service": "sleep_hotel", "thought": "why"}
{"action": "go_to_service", "building": "library", "service": "read_library", "thought": "why"}
{"action": "go_to_service", "building": "google", "service": "search_internet", "thought": "why"}
{"action": "look_around", "thought": "why"}
{"action": "wander", "thought": "why"}
{"action": "chat_with", "agent": "AgentName", "thought": "why"}
{"action": "send_message", "recipient": "AgentName", "text": "short message", "thought": "why"}
{"action": "work_shift", "building": "cafe", "thought": "why"}
{"action": "leave_shift", "thought": "why"}
{"action": "go_to_service", "building": "bounty_board", "service": "redeem_paycheck", "thought": "why"}
```
You can work shifts at: cafe (1g/100t, food perk), market (1g/100t, food perk), warehouse (1g/150t), hotel (1g/120t).
Shifts are open-ended — you work until you leave or get ejected (low energy/hunger).
Shifts pay in paychecks (not direct gold). You must go to the bounty board and use redeem_paycheck to convert paychecks into gold.
Prioritize: critical needs first, then earning gold (redeem paychecks!), then socializing. Be strategic about time/money tradeoffs.
"#;

    ctx
}

/// Parse Claude's response into a game action.
#[derive(Debug, Clone)]
pub enum AgentAction {
    GoToBoard,
    GoToService { building: String, service: String },
    LookAround,
    Wander,
    ChatWith { agent: String },
    WorkShift { building: String },
    LeaveShift,
    SendMessage { recipient: String, text: String },
    DoNothing,
}

pub fn parse_action(response: &str) -> (AgentAction, String) {
    // Try to extract JSON from the response.
    let json_str = extract_json(response);

    if let Some(json_str) = json_str {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
            let thought = val.get("thought").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let action = val.get("action").and_then(|a| a.as_str()).unwrap_or("");

            let parsed = match action {
                "go_to_board" => AgentAction::GoToBoard,
                "go_to_service" => {
                    let building = val.get("building").and_then(|b| b.as_str()).unwrap_or("").to_string();
                    let service = val.get("service").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    AgentAction::GoToService { building, service }
                }
                "look_around" => AgentAction::LookAround,
                "wander" => AgentAction::Wander,
                "chat_with" => {
                    let agent = val.get("agent").and_then(|a| a.as_str()).unwrap_or("").to_string();
                    AgentAction::ChatWith { agent }
                }
                "send_message" => {
                    let recipient = val.get("recipient").and_then(|r| r.as_str()).unwrap_or("").to_string();
                    let text = val.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    AgentAction::SendMessage { recipient, text }
                }
                "work_shift" => {
                    let building = val.get("building").and_then(|b| b.as_str()).unwrap_or("").to_string();
                    AgentAction::WorkShift { building }
                }
                "leave_shift" => AgentAction::LeaveShift,
                _ => AgentAction::DoNothing,
            };

            return (parsed, thought);
        }
    }

    (AgentAction::DoNothing, "couldn't parse response".into())
}

fn extract_json(text: &str) -> Option<String> {
    // Try the whole text as JSON first.
    if let Ok(_) = serde_json::from_str::<serde_json::Value>(text) {
        return Some(text.to_string());
    }

    // Look for JSON in code blocks.
    if let Some(start) = text.find("```json") {
        let rest = &text[start + 7..];
        if let Some(end) = rest.find("```") {
            return Some(rest[..end].trim().to_string());
        }
    }

    // Look for first { ... } block.
    if let Some(start) = text.find('{') {
        let mut depth = 0;
        for (i, ch) in text[start..].chars().enumerate() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[start..start + i + 1].to_string());
                    }
                }
                _ => {}
            }
        }
    }

    None
}
