//! Game engine events sent as user messages to agent Claude sessions.
//! The game engine acts as a "DM" — notifying agents of arrivals, low needs,
//! new bounties, and periodic stat updates via their persistent session channel.

use bevy::prelude::*;

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::{BountyBoard, BountyTokenStore};
use crate::world::map::GridPos;
use crate::world::structures::{Entrance, SpriteType, StructureId};

/// Tracks per-agent state to avoid spamming repeated alerts.
#[derive(Component)]
pub struct LastEventState {
    /// Tick when we last sent a hunger alert.
    pub last_hunger_alert: u64,
    /// Tick when we last sent an energy alert.
    pub last_energy_alert: u64,
    /// Tick when we last sent a periodic stat update.
    pub last_stat_tick: u64,
    /// Number of bounties we last saw in the registry (to detect new ones).
    pub last_bounty_count: usize,
    /// Whether the agent had a non-empty path last tick (to detect arrival).
    pub had_path: bool,
}

impl Default for LastEventState {
    fn default() -> Self {
        Self {
            last_hunger_alert: 0,
            last_energy_alert: 0,
            last_stat_tick: 0,
            last_bounty_count: 0,
            had_path: false,
        }
    }
}

use crate::config;

/// System: ensure all agents have a `LastEventState` component.
pub fn ensure_event_state_system(
    mut commands: Commands,
    agents: Query<Entity, (With<AgentName>, Without<LastEventState>)>,
) {
    for entity in &agents {
        commands.entity(entity).insert(LastEventState::default());
    }
}

/// System: detect game events and send user messages to agent sessions.
pub fn game_events_system(
    tick: Res<TickCount>,
    sessions: Res<AgentSessions>,
    boards_events: Query<&BountyTokenStore, With<BountyBoard>>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &Needs,
        &Inventory,
        &AgentGoal,
        &BusinessCards,
        Option<&Path>,
        &mut LastEventState,
    )>,
    structures: Query<(&Entrance, &SpriteType), With<StructureId>>,
) {
    let Some(bounty_registry) = boards_events.iter().next() else {
        return;
    };
    // Pre-compute: detect new bounties (global, not per-agent).
    let current_bounty_count = bounty_registry.tokens.len();

    for (entity, name, pos, needs, inv, goal, cards, path, mut event_state) in &mut agents {
        let Some(session) = sessions.sessions.get(&entity) else {
            continue;
        };

        let mut messages: Vec<String> = Vec::new();

        // --- Arrival detection ---
        // Agent had a path last tick, but now path is empty (or removed) = arrived.
        let path_is_empty = path.map_or(true, |p| p.0.is_empty());
        if event_state.had_path && path_is_empty {
            // Check if agent is at any building entrance.
            for (entrance, sprite) in &structures {
                if pos.x == entrance.0.x && pos.y == entrance.0.y {
                    let location_info = building_arrival_message(&sprite.0);
                    messages.push(location_info);
                    break;
                }
            }
        }
        event_state.had_path = !path_is_empty;

        // --- Hunger alert ---
        if needs.hunger < config::need_alert_threshold()
            && tick.0.saturating_sub(event_state.last_hunger_alert) >= config::need_alert_cooldown()
        {
            messages.push("You're getting hungry!".to_string());
            event_state.last_hunger_alert = tick.0;
        }

        // --- Energy alert ---
        if needs.energy < config::need_alert_threshold()
            && tick.0.saturating_sub(event_state.last_energy_alert) >= config::need_alert_cooldown()
        {
            messages.push("You're getting tired!".to_string());
            event_state.last_energy_alert = tick.0;
        }

        // --- New bounty detection ---
        if current_bounty_count > event_state.last_bounty_count {
            let new_count = current_bounty_count - event_state.last_bounty_count;
            messages.push(format!(
                "{} new bounty(s) posted! Check the bounty board.",
                new_count
            ));
            event_state.last_bounty_count = current_bounty_count;
        }

        // --- Periodic stat + inventory + quest refresh ---
        if tick.0.saturating_sub(event_state.last_stat_tick) >= config::status_interval() {
            let gold = inv.count(ItemType::GoldCoin);
            let mut status = format!(
                "=== STATUS UPDATE (tick {}) ===\nNeeds: E:{:.0} H:{:.0} B:{:.0}\nGold: {}",
                tick.0, needs.energy, needs.hunger, needs.boredom, gold,
            );

            if inv.gold_debt > 0 {
                status += &format!(" (DEBT: {}g!)", inv.gold_debt);
            }

            // Current goal / quest state.
            match goal {
                AgentGoal::ExecutingBounty(bid) => {
                    if let Some(bounty) = bounty_registry.get(*bid) {
                        status += &format!(
                            "\nACTIVE BOUNTY: '{}' ({}g reward)",
                            bounty.description, bounty.reward_gold
                        );
                        // Show agent-facing instructions.
                        let instructions = bounty
                            .hidden_criteria
                            .split("\n\nGM:")
                            .next()
                            .unwrap_or("")
                            .strip_prefix("Instructions for agent: ")
                            .unwrap_or("");
                        if !instructions.is_empty() {
                            status += &format!("\n  HOW TO COMPLETE: {}", instructions);
                        }
                        status += "\n  When done, return to the bounty board, deposit your bounty_token first, deposit any proof documents/items, then use complete_bounty and wait for the GM verdict.";
                    }
                }
                AgentGoal::Idle => {
                    status += "\nNo active bounty. Go to the bounty board to claim one.";
                }
                _ => {
                    status += &format!("\nCurrent goal: {:?}", goal);
                }
            }

            // Full inventory.
            let items: Vec<String> = inv
                .items
                .iter()
                .filter(|(t, c)| **t != ItemType::GoldCoin && **c > 0)
                .map(|(t, c)| format!("{} x{}", t, c))
                .collect();
            if !items.is_empty() {
                status += &format!("\nInventory: {}", items.join(", "));
            } else {
                status += "\nInventory: empty";
            }

            // Business card contacts.
            if !cards.contacts.is_empty() {
                let names: Vec<&String> = cards.contacts.keys().collect();
                status += &format!(
                    "\nContacts: {} (can send_message to them)",
                    names
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // Walking state.
            if let Some(crate::agents::components::Path(ref path_deque)) = path {
                if !path_deque.is_empty() {
                    status += &format!("\nWalking: {} tiles remaining", path_deque.len());
                }
            }

            status += "\nReminder: Use inspect with service='game_manual' to review the full rules and action list.";
            status += "\nReminder: Use help for the cheat sheet, and include text if you want to file feedback.";
            status += "\n=== END STATUS ===";
            messages.push(status);
            event_state.last_stat_tick = tick.0;
        }

        // --- Send all collected messages ---
        if !messages.is_empty() {
            let combined = messages.join("\n");
            if let Err(e) = session.prompt_tx.try_send(combined.clone()) {
                tracing::debug!(
                    "[GameEvents:{}] failed to send event message: {}",
                    name.0,
                    e
                );
            } else {
                tracing::debug!("[GameEvents:{}] sent: {}", name.0, combined);
            }
        }
    }
}

/// Generate a descriptive arrival message for a building with available commands.
fn building_arrival_message(building_name: &str) -> String {
    match building_name {
        "bounty_board" => "You arrived at the BOUNTY BOARD.\n\
            This is where you find work and get paid.\n\
            Available actions here:\n\
            - claim_bounty — pick a bounty from the board to work on\n\
            - redeem_paycheck — convert any paychecks from shifts into gold\n\
            - deposit_item — deposit your bounty_token and proof items/documents into the dropbox\n\
            - complete_bounty — submit for GM review after your bounty_token and proof are deposited\n\
            - look_around — see nearby agents and buildings\n\
            WORKFLOW: claim_bounty → do the work → deposit_item(bounty_token) → deposit_item(doc:yourfile.md or required proof) → complete_bounty → wait for GM verdict"
            .to_string(),

        "cafe" => "You arrived at the CAFE.\n\
            A cozy spot for food, coffee, and socializing.\n\
            Available services:\n\
            - buy_muffin (1g, 5 ticks) — gives a muffin you can consume for hunger\n\
            - buy_coffee (1g, 5 ticks) — gives coffee you can consume for energy/context\n\
            - hang_out (free, 15 ticks) — +25 boredom\n\
            Available shifts: work_shift (1g per 120 ticks, food perk)"
            .to_string(),

        "hotel" => "You arrived at the HOTEL.\n\
            Rest and recharge here.\n\
            Available services:\n\
            - sleep_hotel (1g, 30 ticks) — +50 energy\n\
            - relax_in_lobby (free, 10 ticks) — +15 boredom, +5 energy\n\
            Available shifts: work_shift (1g per 120 ticks)"
            .to_string(),

        "apartments" => "You arrived at the APARTMENTS.\n\
            Your free home base — no gold required.\n\
            Available services:\n\
            - sleep_at_home (free, 50 ticks) — +80 energy"
            .to_string(),

        "warehouse" => "You arrived at the WAREHOUSE.\n\
            Raw materials are stored here.\n\
            Available shifts: work_shift (1g per 120 ticks)\n\
            Delivery bounties may require picking up items here."
            .to_string(),

        "market" => "You arrived at the MARKET.\n\
            Buy and sell goods.\n\
            Available services:\n\
            - buy_sandwich (2g, 5 ticks) — gives a sandwich you can consume for hunger\n\
            - buy_rations (1g, 5 ticks) — gives rations you can consume for hunger\n\
            - window_shop (free, 10 ticks) — +15 boredom\n\
            Available shifts: work_shift (1g per 120 ticks, food perk)"
            .to_string(),

        "library" => "You arrived at the LIBRARY.\n\
            Archive of all completed bounty research documents.\n\
            Available actions:\n\
            - inspect (free) — browse the catalog here, or inspect service=<title> to read a document\n\
            - copy_document (free) — copy a document to your inventory (use service=title)\n\
            You cannot take originals, only copies."
            .to_string(),

        "google" => "You arrived at GOOGLE.\n\
            The internet is here. Costs 1 gold per visit.\n\
            Available services:\n\
            - search_internet (1g, 10 ticks) — real web search for bounty research"
            .to_string(),

        "hospital" => "You arrived at the HOSPITAL.\n\
            You end up here if you collapse from critical needs.\n\
            Recovery costs 5 gold (can go into debt)."
            .to_string(),

        other => format!("You arrived at {}.", other),
    }
}

#[cfg(test)]
mod tests {
    use super::building_arrival_message;

    #[test]
    fn bounty_board_arrival_message_uses_current_submission_flow() {
        let message = building_arrival_message("bounty_board");

        assert!(message.contains("redeem_paycheck"));
        assert!(message.contains("deposit_item(bounty_token)"));
        assert!(message.contains("wait for GM verdict"));
        assert!(!message.contains("collect your reward"));
    }

    #[test]
    fn cafe_arrival_message_lists_real_food_services() {
        let message = building_arrival_message("cafe");

        assert!(message.contains("buy_muffin"));
        assert!(message.contains("buy_coffee"));
        assert!(!message.contains("eat_cafe"));
    }

    #[test]
    fn market_arrival_message_lists_real_food_services() {
        let message = building_arrival_message("market");

        assert!(message.contains("buy_sandwich"));
        assert!(message.contains("buy_rations"));
    }
}
