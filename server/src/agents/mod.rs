pub mod action_log;
pub mod actions;
pub mod ai;
pub mod ai_chat;
pub mod ai_decision;
pub mod behavior;
pub mod claude;
pub mod components;
pub mod event_log;
pub mod game_events;
pub mod mailbox;
pub mod movement;
pub mod needs;
pub mod pathfinding;
pub mod perception;
pub mod personality;
pub mod social;
pub mod summarizer;
pub mod thinking_log;

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
            .init_resource::<thinking_log::ThinkingLogCursor>()
            .add_systems(Startup, spawn_agents.after(init_map))
            .add_systems(
                Update,
                (
                    (
                        needs::needs_decay_system,
                        movement::movement_system,
                        actions::action_timer_system,
                        perception::tracking_update_system,
                        perception::look_around_system,
                        perception::start_tracking_system,
                        perception::inspect_system,
                        perception::passive_discovery_system,
                        // social::social_matchmaking_system, // DISABLED: chatting via MCP tool only
                        social::social_memory_system,
                    ).chain(),
                    (
                        ai::spawn_sessions_system,
                        // ai_chat::ai_chat_system, // DISABLED: chatting via MCP tool only
                        ai::ai_thought_drain_system,
                    ai::ai_context_system,
                        summarizer::summarize_thoughts_system,
                        behavior::execution_system,
                        mailbox::process_outgoing_mail_system,
                        mailbox::deliver_mail_system,
                        thinking_log::capture_thinking_system,
                        thinking_log::flush_thinking_log_system,
                        game_events::ensure_event_state_system,
                        game_events::game_events_system,
                    ).chain(),
                )
                    .chain(),
            );
    }
}

fn spawn_agents(
    mut commands: Commands,
    map: Res<WorldMap>,
    boards: Query<(Entity, &GridPos, &Entrance), With<BountyBoard>>,
    all_structures: Query<(Entity, &GridPos, &Entrance, &crate::world::structures::SpriteType)>,
) {
    let walkable = map.walkable_positions();
    // Each agent gets a different Claude model to test speed vs intelligence.
    let agents_config: Vec<(&str, f32, &str)> = vec![
        ("Alice", 2.0, "haiku"),   // fastest model, fastest speed
        ("Bob", 1.5, "sonnet"),    // balanced model, moderate speed
        ("Carol", 3.0, "opus"),    // smartest model, fastest legs
    ];

    let board_info: Option<(Entity, GridPos, GridPos)> = boards
        .iter()
        .next()
        .map(|(e, pos, entrance)| (e, *pos, entrance.0));

    // Spawn agents near the bounty board so they know where to start.
    let board_pos = board_info.map(|(_, _, entrance)| entrance).unwrap_or(GridPos { x: 20, y: 32 });
    for (i, (name, speed, model)) in agents_config.iter().enumerate() {
        let offset = (i as i32 - 1) * 2;
        let start = GridPos { x: board_pos.x + offset, y: board_pos.y - 2 };
        let mut bundle = AgentBundle::new(name, start, *speed);

        // Seed known locations: bounty board + apartments.
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
        // Seed all buildings as known (agents are residents of the city).
        for (e, bpos, entrance, sprite) in &all_structures {
            bundle.known_locations.locations.insert(
                e,
                KnownPlace {
                    name: sprite.0.clone(),
                    pos: *bpos,
                    entrance: entrance.0,
                    discovered_tick: 0,
                    source: DiscoverySource::Initial,
                },
            );
        }

        // Generate unique personality.
        let personality = personality::generate_personality(name);
        let claude_model = components::ClaudeModel(model.to_string());
        tracing::info!("spawned {name} at ({}, {}), speed={speed}, model={model}\n{}", start.x, start.y, personality.traits);

        commands.spawn((bundle, personality, claude_model));
    }
}
