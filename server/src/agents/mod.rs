pub mod action_log;
pub mod actions;
pub mod behavior;
pub mod components;
pub mod movement;
pub mod needs;
pub mod pathfinding;
pub mod perception;
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
        app.add_systems(Startup, spawn_agents.after(init_map))
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
                    behavior::agent_behavior_system,
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

    // Get the bounty board entity so agents start knowing where it is.
    let board_info: Option<(Entity, GridPos, GridPos)> = boards.iter().next()
        .map(|(e, pos, entrance)| (e, *pos, entrance.0));

    for (i, (name, speed)) in agents_config.iter().enumerate() {
        let start = walkable[i * 17 % walkable.len()];
        let mut bundle = AgentBundle::new(name, start, *speed);

        // Seed: every agent knows where the bounty board is.
        if let Some((board_entity, board_pos, board_entrance)) = board_info {
            bundle.known_locations.locations.insert(board_entity, KnownPlace {
                name: "bounty_board".into(),
                pos: board_pos,
                entrance: board_entrance,
                discovered_tick: 0,
                source: DiscoverySource::Initial,
            });
        }

        commands.spawn(bundle);
        tracing::info!("spawned {name} at ({}, {}), speed={speed}", start.x, start.y);
    }
}
