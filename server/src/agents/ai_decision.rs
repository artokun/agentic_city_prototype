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
    active_bounty: Option<&str>,
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
- Need levels: >50 is fine, 25-50 is low, <25 is urgent, <10 is CRITICAL (auto-handled)
- DO NOT worry about needs above 25. Gold under 10 is low but not critical. Focus on earning gold.
"#,
        pos.x, pos.y, needs.energy, needs.hunger, needs.boredom, goal,
    );

    // Gold balance with debt info
    if inv.gold_debt > 0 {
        ctx += &format!("- DEBT: {} gold owed! Earn gold to pay it off.\n", inv.gold_debt);
    }

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

    // Active bounty (if executing one)
    if let Some(bounty_desc) = active_bounty {
        ctx += "\n## YOUR ACTIVE BOUNTY\n";
        ctx += &format!("You are currently working on: {}\n", bounty_desc);
        ctx += "You must figure out HOW to complete this bounty. Go to buildings, search, interact.\n";
        ctx += "When done, use 'go_to_board' to return and collect your reward.\n";
        ctx += "Use 'complete_bounty' when you believe the objective is fulfilled.\n";
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

    // Service price list
    ctx += r#"
## Service Costs & Effects
| Service | Building | Cost | Duration | Effect |
|---------|----------|------|----------|--------|
| eat_cafe | cafe | 1g | 10 ticks | +40 hunger |
| buy_coffee | cafe | 1g | 5 ticks | +20 energy (instant boost!) |
| cook_at_home | apartments | FREE | 30 ticks | +60 hunger |
| sleep_hotel | hotel | 1g | 30 ticks | +50 energy |
| sleep_at_home | apartments | FREE | 50 ticks | +80 energy |
| redeem_paycheck | bounty_board | FREE | 5 ticks | converts paychecks to gold |

## Shift Pay Rates
| Building | Pay Rate | Food Perk |
|----------|----------|-----------|
| cafe | 1g per 1000 ticks | Yes (hunger stable) |
| market | 1g per 1000 ticks | Yes |
| warehouse | 1g per 1200 ticks | No |
| hotel | 1g per 1100 ticks | No |

Shifts pay in paychecks. You must go to bounty_board to redeem_paycheck for gold.
Bounties pay 4-15g and are much faster than shifts for earning gold.
The apartments are FREE for sleep and food but far away — hotel costs 1g but is closer.

## Respond with ONE JSON action:
```json
{"action": "go_to_board", "thought": "why"}
{"action": "go_to_service", "building": "cafe", "service": "eat_cafe", "thought": "why"}
{"action": "go_to_service", "building": "cafe", "service": "buy_coffee", "thought": "why"}
{"action": "go_to_service", "building": "hotel", "service": "sleep_hotel", "thought": "why"}
{"action": "go_to_service", "building": "apartments", "service": "sleep_at_home", "thought": "why"}
{"action": "go_to_service", "building": "apartments", "service": "cook_at_home", "thought": "why"}
{"action": "go_to_service", "building": "bounty_board", "service": "redeem_paycheck", "thought": "why"}
{"action": "look_around", "thought": "why"}
{"action": "wander", "thought": "why"}
{"action": "work_shift", "building": "cafe", "thought": "why"}
{"action": "leave_shift", "thought": "why"}
{"action": "complete_bounty", "thought": "why"}
```
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
    CompleteBounty,
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
                "complete_bounty" => AgentAction::CompleteBounty,
                _ => AgentAction::DoNothing,
            };

            return (parsed, thought);
        }
    }

    // Fallback: try to infer action from prose keywords.
    let lower = response.to_lowercase();
    let thought = response.lines().next().unwrap_or("thinking...").to_string();

    if lower.contains("bounty board") || lower.contains("check the board") || lower.contains("go to the board") {
        return (AgentAction::GoToBoard, thought);
    }
    if lower.contains("work a shift") || lower.contains("take a shift") || lower.contains("start working") {
        if lower.contains("cafe") {
            return (AgentAction::WorkShift { building: "cafe".into() }, thought);
        }
        if lower.contains("market") {
            return (AgentAction::WorkShift { building: "market".into() }, thought);
        }
        if lower.contains("warehouse") {
            return (AgentAction::WorkShift { building: "warehouse".into() }, thought);
        }
    }
    if lower.contains("look around") || lower.contains("scan") || lower.contains("survey") {
        return (AgentAction::LookAround, thought);
    }
    if lower.contains("wander") || lower.contains("explore") || lower.contains("walk around") {
        return (AgentAction::Wander, thought);
    }
    if lower.contains("complete") && lower.contains("bounty") {
        return (AgentAction::CompleteBounty, thought);
    }

    (AgentAction::Wander, format!("(no JSON) {}", thought))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json_go_to_board() {
        let input = r#"{"action": "go_to_board", "thought": "need bounties"}"#;
        let (action, thought) = parse_action(input);
        assert!(matches!(action, AgentAction::GoToBoard));
        assert_eq!(thought, "need bounties");
    }

    #[test]
    fn parse_json_in_code_block() {
        let input = r#"I think I should go to the board.
```json
{"action": "go_to_board", "thought": "checking for work"}
```
That's my plan."#;
        let (action, thought) = parse_action(input);
        assert!(matches!(action, AgentAction::GoToBoard));
        assert_eq!(thought, "checking for work");
    }

    #[test]
    fn parse_go_to_service() {
        let input = r#"{"action": "go_to_service", "building": "cafe", "service": "eat_cafe", "thought": "hungry"}"#;
        let (action, thought) = parse_action(input);
        match action {
            AgentAction::GoToService { building, service } => {
                assert_eq!(building, "cafe");
                assert_eq!(service, "eat_cafe");
            }
            other => panic!("expected GoToService, got {:?}", other),
        }
        assert_eq!(thought, "hungry");
    }

    #[test]
    fn parse_work_shift() {
        let input = r#"{"action": "work_shift", "building": "warehouse", "thought": "earn gold"}"#;
        let (action, thought) = parse_action(input);
        match action {
            AgentAction::WorkShift { building } => {
                assert_eq!(building, "warehouse");
            }
            other => panic!("expected WorkShift, got {:?}", other),
        }
        assert_eq!(thought, "earn gold");
    }

    #[test]
    fn parse_send_message() {
        let input = r#"{"action": "send_message", "recipient": "Alice", "text": "hello!", "thought": "being friendly"}"#;
        let (action, thought) = parse_action(input);
        match action {
            AgentAction::SendMessage { recipient, text } => {
                assert_eq!(recipient, "Alice");
                assert_eq!(text, "hello!");
            }
            other => panic!("expected SendMessage, got {:?}", other),
        }
        assert_eq!(thought, "being friendly");
    }

    #[test]
    fn parse_look_around() {
        let input = r#"{"action": "look_around", "thought": "exploring"}"#;
        let (action, _) = parse_action(input);
        assert!(matches!(action, AgentAction::LookAround));
    }

    #[test]
    fn parse_wander() {
        let input = r#"{"action": "wander", "thought": "bored"}"#;
        let (action, _) = parse_action(input);
        assert!(matches!(action, AgentAction::Wander));
    }

    #[test]
    fn parse_chat_with() {
        let input = r#"{"action": "chat_with", "agent": "Bob", "thought": "socialize"}"#;
        let (action, _) = parse_action(input);
        match action {
            AgentAction::ChatWith { agent } => assert_eq!(agent, "Bob"),
            other => panic!("expected ChatWith, got {:?}", other),
        }
    }

    #[test]
    fn parse_leave_shift() {
        let input = r#"{"action": "leave_shift", "thought": "tired"}"#;
        let (action, _) = parse_action(input);
        assert!(matches!(action, AgentAction::LeaveShift));
    }

    #[test]
    fn parse_unknown_action_returns_do_nothing() {
        let input = r#"{"action": "fly_away", "thought": "imagination"}"#;
        let (action, _) = parse_action(input);
        assert!(matches!(action, AgentAction::DoNothing));
    }

    #[test]
    fn garbage_input_falls_back_to_wander() {
        // Prose fallback: unrecognized text defaults to Wander
        let (action, _) = parse_action("this is not json at all!!!");
        assert!(matches!(action, AgentAction::Wander));
    }

    #[test]
    fn empty_input_falls_back_to_wander() {
        let (action, _) = parse_action("");
        assert!(matches!(action, AgentAction::Wander));
    }

    #[test]
    fn json_embedded_in_prose() {
        let input = r#"After thinking carefully, I've decided: {"action": "wander", "thought": "need to explore"} and that's my plan."#;
        let (action, thought) = parse_action(input);
        assert!(matches!(action, AgentAction::Wander));
        assert_eq!(thought, "need to explore");
    }

    #[test]
    fn missing_thought_field() {
        let input = r#"{"action": "go_to_board"}"#;
        let (action, thought) = parse_action(input);
        assert!(matches!(action, AgentAction::GoToBoard));
        assert_eq!(thought, "");
    }
}
