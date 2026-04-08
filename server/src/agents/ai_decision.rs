#![allow(dead_code)]

use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::world::map::GridPos;

/// Build the decision context for an agent to send to Claude.
/// This is a slim vitals-only update. Agents can use check_own_stats, check_inventory,
/// check_known_locations, and check_relationships to query their full state on demand.
pub fn build_context(
    _name: &str,
    pos: &GridPos,
    needs: &Needs,
    inv: &Inventory,
    goal: &AgentGoal,
    tick: u64,
    available_bounties: &[String],
    nearby_agents: &[(String, GridPos)],
    location_tools: &[&str],
    active_bounty: Option<&str>,
) -> String {
    let gold = inv.count(ItemType::GoldCoin);

    let mut ctx = format!(
        "--- tick {tick} ---\n\
         Position: ({}, {})\n\
         Energy: {:.0}  Hunger: {:.0}  Boredom: {:.0}  Gold: {gold}\n\
         Goal: {:?}\n",
        pos.x, pos.y, needs.energy, needs.hunger, needs.boredom, goal,
    );

    if inv.gold_debt > 0 {
        ctx += &format!("DEBT: {}g\n", inv.gold_debt);
    }

    // Need alerts
    if needs.energy < 25.0 {
        ctx += "WARNING: Energy low!\n";
    }
    if needs.hunger < 25.0 {
        ctx += "WARNING: Hunger low!\n";
    }
    if needs.boredom < 10.0 {
        ctx += "WARNING: Boredom critical!\n";
    }

    // Active bounty (if executing one)
    if let Some(bounty_desc) = active_bounty {
        ctx += &format!("\nACTIVE BOUNTY: {}\n", bounty_desc);
    }

    // Available bounties (only if at board or own bounty)
    if !available_bounties.is_empty() {
        ctx += "\nBounties:\n";
        for b in available_bounties {
            ctx += &format!("- {}\n", b);
        }
    }

    // Nearby agents
    if !nearby_agents.is_empty() {
        ctx += "\nNearby:";
        for (aname, apos) in nearby_agents {
            let dist = (apos.x - pos.x).abs() + (apos.y - pos.y).abs();
            ctx += &format!(" {}({}t)", aname, dist);
        }
        ctx += "\n";
    }

    // Location-specific tools
    if !location_tools.is_empty() {
        ctx += "\nTools here:";
        for tool in location_tools {
            ctx += &format!(" {}", tool);
        }
        ctx += "\n";
    }

    ctx += "\nUse check_own_stats, check_inventory, check_known_locations, and check_relationships for details.\n";

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
            let thought = val
                .get("thought")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let action = val.get("action").and_then(|a| a.as_str()).unwrap_or("");

            let parsed = match action {
                "go_to_board" => AgentAction::GoToBoard,
                "go_to_service" => {
                    let building = val
                        .get("building")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string();
                    let service = val
                        .get("service")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    AgentAction::GoToService { building, service }
                }
                "look_around" => AgentAction::LookAround,
                "wander" => AgentAction::Wander,
                "chat_with" => {
                    let agent = val
                        .get("agent")
                        .and_then(|a| a.as_str())
                        .unwrap_or("")
                        .to_string();
                    AgentAction::ChatWith { agent }
                }
                "send_message" => {
                    let recipient = val
                        .get("recipient")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = val
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    AgentAction::SendMessage { recipient, text }
                }
                "work_shift" => {
                    let building = val
                        .get("building")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string();
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

    if lower.contains("bounty board")
        || lower.contains("check the board")
        || lower.contains("go to the board")
    {
        return (AgentAction::GoToBoard, thought);
    }
    if lower.contains("work a shift")
        || lower.contains("take a shift")
        || lower.contains("start working")
    {
        if lower.contains("cafe") {
            return (
                AgentAction::WorkShift {
                    building: "cafe".into(),
                },
                thought,
            );
        }
        if lower.contains("market") {
            return (
                AgentAction::WorkShift {
                    building: "market".into(),
                },
                thought,
            );
        }
        if lower.contains("warehouse") {
            return (
                AgentAction::WorkShift {
                    building: "warehouse".into(),
                },
                thought,
            );
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

    // Look for first { ... } block (using byte offsets, not char indices).
    if let Some(start) = text.find('{') {
        let mut depth = 0;
        let mut byte_offset = 0;
        for ch in text[start..].chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[start..start + byte_offset + ch.len_utf8()].to_string());
                    }
                }
                _ => {}
            }
            byte_offset += ch.len_utf8();
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context(
        location_tools: &[&str],
        active_bounty: Option<&str>,
    ) -> String {
        build_context(
            "Alice",
            &GridPos { x: 1, y: 2 },
            &Needs::default(),
            &Inventory::default(),
            &AgentGoal::Idle,
            100,
            &[],
            &[],
            location_tools,
            active_bounty,
        )
    }

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
        let input =
            r#"{"action": "go_to_service", "building": "cafe", "service": "buy_muffin", "thought": "hungry"}"#;
        let (action, thought) = parse_action(input);
        match action {
            AgentAction::GoToService { building, service } => {
                assert_eq!(building, "cafe");
                assert_eq!(service, "buy_muffin");
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

    #[test]
    fn parse_unicode_response_no_panic() {
        // Regression: slicing multi-byte UTF-8 at byte boundary caused panics.
        let response = "🤔 Let me think about this... {\"action\": \"go_to_board\", \"thought\": \"I need gold! 💰\"}";
        let (action, _thought) = parse_action(response);
        assert!(matches!(action, AgentAction::GoToBoard));
    }

    #[test]
    fn parse_emoji_heavy_response() {
        let response = "😊🎉🏆💪 {\"action\": \"wander\", \"thought\": \"exploring! 🗺️\"}";
        let (action, _) = parse_action(response);
        assert!(matches!(action, AgentAction::Wander));
    }

    #[test]
    fn build_context_shows_location_tools() {
        let context = sample_context(&["redeem_paycheck"], None);
        assert!(context.contains("redeem_paycheck"));
    }

    #[test]
    fn build_context_shows_active_bounty() {
        let context = sample_context(&["complete_bounty"], Some("Visit every building"));
        assert!(context.contains("ACTIVE BOUNTY: Visit every building"));
    }

    #[test]
    fn build_context_mentions_check_tools() {
        let context = sample_context(&[], None);
        assert!(context.contains("check_own_stats"));
        assert!(context.contains("check_inventory"));
        assert!(context.contains("check_known_locations"));
        assert!(context.contains("check_relationships"));
    }
}
