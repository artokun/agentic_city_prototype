//! Game engine events sent as user messages to agent Claude sessions.
//! The game engine acts as a "DM" — notifying agents of arrivals, low needs,
//! new bounties, and periodic stat updates via their persistent session channel.

use bevy::prelude::*;

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::BountyRegistry;
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

/// Cooldown in ticks before re-sending a need alert for the same stat.
const NEED_ALERT_COOLDOWN: u64 = 200;

/// Threshold below which we alert the agent about a need.
const NEED_ALERT_THRESHOLD: f32 = 30.0;

/// Interval in ticks between periodic stat updates.
const STAT_REFRESH_INTERVAL: u64 = 200;

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
    bounty_registry: Res<BountyRegistry>,
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
    // Pre-compute: detect new bounties (global, not per-agent).
    let current_bounty_count = bounty_registry.bounties.len();

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
        if needs.hunger < NEED_ALERT_THRESHOLD
            && tick.0.saturating_sub(event_state.last_hunger_alert) >= NEED_ALERT_COOLDOWN
        {
            messages.push("You're getting hungry!".to_string());
            event_state.last_hunger_alert = tick.0;
        }

        // --- Energy alert ---
        if needs.energy < NEED_ALERT_THRESHOLD
            && tick.0.saturating_sub(event_state.last_energy_alert) >= NEED_ALERT_COOLDOWN
        {
            messages.push("You're getting tired!".to_string());
            event_state.last_energy_alert = tick.0;
        }

        // --- New bounty detection ---
        if current_bounty_count > event_state.last_bounty_count {
            // Send descriptions of all new bounties.
            for bounty in bounty_registry.bounties.iter().skip(event_state.last_bounty_count) {
                messages.push(format!("New bounty posted: {}", bounty.description));
            }
            event_state.last_bounty_count = current_bounty_count;
        }

        // --- Periodic stat + inventory + quest refresh ---
        if tick.0.saturating_sub(event_state.last_stat_tick) >= STAT_REFRESH_INTERVAL {
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
                        status += &format!("\nACTIVE BOUNTY: '{}' ({}g reward)", bounty.description, bounty.reward_gold);
                        // Show agent-facing instructions.
                        let instructions = bounty.hidden_criteria
                            .split("\n\nGM:")
                            .next()
                            .unwrap_or("")
                            .strip_prefix("Instructions for agent: ")
                            .unwrap_or("");
                        if !instructions.is_empty() {
                            status += &format!("\n  HOW TO COMPLETE: {}", instructions);
                        }
                        status += "\n  When done, go to the bounty board and use complete_bounty.";
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
            let items: Vec<String> = inv.items.iter()
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
                status += &format!("\nContacts: {} (can send_message to them)", names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
            }

            // Walking state.
            if let Some(crate::agents::components::Path(ref path_deque)) = path {
                if !path_deque.is_empty() {
                    status += &format!("\nWalking: {} tiles remaining", path_deque.len());
                }
            }

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
            - complete_bounty — if you have an active bounty, return here to collect your reward\n\
            - look_around — see nearby agents and buildings".to_string(),

        "cafe" => "You arrived at the CAFE.\n\
            A cozy spot for food, coffee, and socializing.\n\
            Available services:\n\
            - eat_cafe (1g, 10 ticks) — +40 hunger\n\
            - buy_coffee (1g, 5 ticks) — +20 energy\n\
            - hang_out (free, 15 ticks) — +25 boredom\n\
            Available shifts: work_shift (1g per 1000 ticks, food perk)".to_string(),

        "hotel" => "You arrived at the HOTEL.\n\
            Rest and recharge here.\n\
            Available services:\n\
            - sleep_hotel (1g, 30 ticks) — +50 energy\n\
            - relax_in_lobby (free, 10 ticks) — +15 boredom, +5 energy\n\
            Available shifts: work_shift (1g per 1100 ticks)".to_string(),

        "apartments" => "You arrived at the APARTMENTS.\n\
            Your free home base — no gold required.\n\
            Available services:\n\
            - sleep_at_home (free, 50 ticks) — +80 energy\n\
            - cook_at_home (free, 30 ticks) — +60 hunger".to_string(),

        "warehouse" => "You arrived at the WAREHOUSE.\n\
            Raw materials are stored here.\n\
            Available shifts: work_shift (1g per 1200 ticks)\n\
            Delivery bounties may require picking up items here.".to_string(),

        "market" => "You arrived at the MARKET.\n\
            Buy and sell goods.\n\
            Available services:\n\
            - window_shop (free, 10 ticks) — +15 boredom\n\
            Available shifts: work_shift (1g per 1000 ticks, food perk)".to_string(),

        "google" => "You arrived at GOOGLE.\n\
            The internet is here. Costs 1 gold per visit.\n\
            Available services:\n\
            - search_internet (1g, 10 ticks) — real web search for bounty research".to_string(),

        "library" => "You arrived at the LIBRARY.\n\
            Quiet place to read and relax.\n\
            Available services:\n\
            - read_library (free, 20 ticks) — +30 boredom".to_string(),

        "theater" => "You arrived at the THEATER.\n\
            Entertainment venue.\n\
            Available services:\n\
            - watch_show (2g, 25 ticks) — +50 boredom".to_string(),

        "gym" => "You arrived at the GYM.\n\
            Work out to relieve boredom.\n\
            Available services:\n\
            - work_out (free, 20 ticks) — +25 boredom, -10 energy, -10 hunger".to_string(),

        "hospital" => "You arrived at the HOSPITAL.\n\
            You end up here if you collapse from critical needs.\n\
            Recovery costs 5 gold (can go into debt).".to_string(),

        "diner" => "You arrived at the DINER.\n\
            Quick and cheap eats.\n\
            Available services:\n\
            - eat_diner (1g, 12 ticks) — +45 hunger".to_string(),

        "restaurant" => "You arrived at the RESTAURANT.\n\
            Fine dining with social benefits.\n\
            Available services:\n\
            - eat_restaurant (2g, 15 ticks) — +60 hunger, +10 boredom".to_string(),

        other => format!("You arrived at {}.", other),
    }
}
