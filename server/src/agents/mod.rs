pub mod action_log;
pub mod actions;
pub mod ai;
pub mod ai_decision;
pub mod behavior;
pub mod claude;
pub mod components;
pub mod event_log;
pub mod movement;
pub mod needs;
pub mod pathfinding;
pub mod perception;
pub mod personality;
pub mod social;

use bevy::prelude::*;

use crate::world::bounty::BountyBoard;
use crate::world::map::{init_map, GridPos, WorldMap};
use crate::world::structures::Entrance;
use components::*;
use perception::{DiscoverySource, KnownPlace};

pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ai::AgentSessions>()
            .init_resource::<event_log::AgentEventLog>()
            .add_systems(Startup, spawn_agents.after(init_map))
            .add_systems(
                Update,
                (
                    needs::needs_decay_system,
                    movement::movement_system,
                    actions::action_timer_system,
                    perception::tracking_update_system,
                    perception::look_around_system,
                    perception::start_tracking_system,
                    perception::inspect_system,
                    perception::passive_discovery_system,
                    social::social_matchmaking_system,
                    social::social_memory_system,
                    ai::spawn_sessions_system,
                    ai::ai_decision_system,
                    behavior::execution_system,
                )
                    .chain(),
            );
    }
}

fn spawn_agents(
    mut commands: Commands,
    map: Res<WorldMap>,
    boards: Query<(Entity, &GridPos, &Entrance), With<BountyBoard>>,
) {
    let walkable = map.walkable_positions();
    let agents_config = [
        ("Alice", 2.0_f32),
        ("Bob", 1.5),
        ("Carol", 3.0),
    ];

    let board_info: Option<(Entity, GridPos, GridPos)> = boards
        .iter()
        .next()
        .map(|(e, pos, entrance)| (e, *pos, entrance.0));

    for (i, (name, speed)) in agents_config.iter().enumerate() {
        let start = walkable[i * 17 % walkable.len()];
        let mut bundle = AgentBundle::new(name, start, *speed);

        // Seed bounty board location.
        if let Some((board_entity, board_pos, board_entrance)) = board_info {
            bundle.known_locations.locations.insert(
                board_entity,
                KnownPlace {
                    name: "bounty_board".into(),
                    pos: board_pos,
                    entrance: board_entrance,
                    discovered_tick: 0,
                    source: DiscoverySource::Initial,
                },
            );
        }

        // Generate unique personality.
        let personality = personality::generate_personality(name);
        tracing::info!("spawned {name} at ({}, {}), speed={speed}\n{}", start.x, start.y, personality.traits);

        commands.spawn((bundle, personality));
    }
}
