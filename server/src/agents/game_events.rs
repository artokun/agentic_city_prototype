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
        Option<&Path>,
        &mut LastEventState,
    )>,
    structures: Query<(&Entrance, &SpriteType), With<StructureId>>,
) {
    // Pre-compute: detect new bounties (global, not per-agent).
    let current_bounty_count = bounty_registry.bounties.len();

    for (entity, name, pos, needs, inv, path, mut event_state) in &mut agents {
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
                    messages.push(format!("You arrived at {}.", sprite.0));
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

        // --- Periodic stat refresh ---
        if tick.0.saturating_sub(event_state.last_stat_tick) >= STAT_REFRESH_INTERVAL {
            let gold = inv.count(ItemType::GoldCoin);
            messages.push(format!(
                "Status update: E:{:.0} H:{:.0} B:{:.0} Gold:{}",
                needs.energy, needs.hunger, needs.boredom, gold,
            ));
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
