pub mod action_log;
pub mod actions;
pub mod ai;
pub mod ai_chat;
pub mod ai_decision;
pub mod behavior;
pub mod components;
pub mod conversation;
pub mod event_log;
pub mod game_events;
pub mod gm;
pub mod mailbox;
pub mod movement;
pub mod needs;
pub mod pathfinding;
pub mod perception;
pub mod personality;
pub mod social;
pub mod summarizer;
pub mod thinking_log;
pub mod token_tracking;
pub mod trading;

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
            .init_resource::<gm::SystemAiState>()
            .init_resource::<token_tracking::TokenEventQueue>()
            .add_systems(Startup, spawn_agents.after(init_map))
            .add_systems(
                Update,
                (
                    (
                        needs::needs_decay_system,
                        needs::auto_eat_system,
                        movement::movement_system,
                        actions::action_timer_system,
                        perception::tracking_update_system,
                        perception::look_around_system,
                        perception::start_tracking_system,
                        perception::inspect_system,
                        perception::passive_discovery_system,
                        // social::social_matchmaking_system, // DISABLED: chatting via MCP tool only
                        social::social_memory_system,
                    )
                        .chain(),
                    (
                        ai::spawn_sessions_system,
                        // ai_chat::ai_chat_system, // DISABLED: chatting via MCP tool only
                        ai::ai_thought_drain_system,
                        token_tracking::token_drain_system,
                        ai::ai_context_system,
                        summarizer::summarize_thoughts_system,
                        behavior::execution_system,
                        mailbox::process_outgoing_mail_system,
                        mailbox::deliver_mail_system,
                        thinking_log::capture_thinking_system,
                        thinking_log::flush_thinking_log_system,
                        game_events::ensure_event_state_system,
                        game_events::game_events_system,
                        trading::trade_system,
                        gm::spawn_system_ai_session_system,
                        gm::enqueue_gm_reviews_system,
                        gm::dispatch_system_ai_reviews_system,
                        gm::system_ai_response_drain_system,
                        gm::system_ai_token_drain_system,
                        gm::spawn_research_system,
                        gm::gm_autonomous_monitoring_system,
                    )
                        .chain(),
                )
                    .chain(),
            );
    }
}

fn spawn_agents(
    mut commands: Commands,
    map: Res<WorldMap>,
    boards: Query<(Entity, &GridPos, &Entrance), With<BountyBoard>>,
    all_structures: Query<(
        Entity,
        &GridPos,
        &Entrance,
        &crate::world::structures::SpriteType,
    )>,
    scenario_config: Option<Res<crate::scenario::ScenarioAgentConfig>>,
) {
    let _walkable = map.walkable_positions();

    // Generate game manual by calling mcp-game --generate-manual.
    let manual_content = generate_game_manual_from_mcp();

    // If scenario config is provided, use it; otherwise use defaults.
    let agents_config: Vec<(&str, f32, &str)>;
    let scenario_agents: Vec<(String, f32, String)>;

    if let Some(ref config) = scenario_config {
        scenario_agents = config
            .agents
            .iter()
            .map(|a| (a.name.clone(), a.speed, a.model.clone()))
            .collect();
        agents_config = scenario_agents
            .iter()
            .map(|(n, s, m)| (n.as_str(), *s, m.as_str()))
            .collect();
    } else {
        // Default: each agent gets a profile from config/llm.toml.
        // The profile determines both the provider and model.
        agents_config = vec![
            ("Alice Haiku", 2.0, "agent-default"),  // bubbly socialite
            ("Bob Sonnet", 1.5, "agent-default"),    // no-nonsense ex-military
            ("Carol Opus", 3.0, "agent-smart"),      // hacker gamer girl
            ("Dave GPT", 2.0, "agent-openai"),       // meek, apologetic
        ];
    }

    let board_info: Option<(Entity, GridPos, GridPos)> = boards
        .iter()
        .next()
        .map(|(e, pos, entrance)| (e, *pos, entrance.0));

    // Spawn agents near the bounty board so they know where to start.
    let board_pos = board_info
        .map(|(_, _, entrance)| entrance)
        .unwrap_or(GridPos { x: 20, y: 32 });
    for (i, (name, speed, model)) in agents_config.iter().enumerate() {
        let offset = (i as i32 - 1) * 2;
        let start = GridPos {
            x: board_pos.x + offset,
            y: board_pos.y - 2,
        };
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

        // Give game manual as a starting document.
        bundle.documents.add("game_manual".to_string(), manual_content.clone());

        // Generate unique personality.
        let personality = personality::generate_personality(name);
        // model is now a profile name from config/llm.toml.
        let profile_ref = components::SessionProfileRef(model.to_string());
        tracing::info!(
            "spawned {name} at ({}, {}), speed={speed}, profile={}\n{}",
            start.x,
            start.y,
            profile_ref.0,
            personality.traits
        );

        commands.spawn((bundle, personality, profile_ref));
    }
}

/// Generate game manual by calling `mcp-game --generate-manual`.
/// The MCP binary is the source of truth for all action definitions.
/// Falls back to a minimal manual if the binary isn't found.
fn generate_game_manual_from_mcp() -> String {
    use crate::world::map::{city_buildings, MAP_WIDTH, MAP_HEIGHT};
    use crate::world::services::all_services;

    // Call mcp-game to get the action reference.
    let mcp_manual = std::process::Command::new("./target/debug/mcp-game")
        .arg("--generate-manual")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        });

    let mut manual = String::new();

    // World info from server-side data.
    manual += &format!("# Game Manual\n\n## World\nMap: {}x{} (x: 0-{}, y: 0-{})\n\n",
        MAP_WIDTH, MAP_HEIGHT, MAP_WIDTH - 1, MAP_HEIGHT - 1);

    manual += "## Buildings\n";
    for bld in &city_buildings() {
        let entrance = bld.entrance_pos();
        manual += &format!("- **{}** at ({},{}) entrance ({},{})\n",
            bld.name, bld.x, bld.y, entrance.x, entrance.y);
    }
    manual += "\n";

    // Services from server-side data.
    manual += "## Services (go_to the entrance first, then call the service action directly)\n";
    manual += "| Service | Building | Cost | Duration | Effect |\n";
    manual += "|---------|----------|------|----------|--------|\n";
    for svc in &all_services() {
        let mut effects = Vec::new();
        if svc.effects.energy != 0.0 { effects.push(format!("{:+.0} energy", svc.effects.energy)); }
        if svc.effects.hunger != 0.0 { effects.push(format!("{:+.0} hunger", svc.effects.hunger)); }
        if svc.effects.boredom != 0.0 { effects.push(format!("{:+.0} boredom", svc.effects.boredom)); }
        if let Some(item) = svc.produces_item { effects.push(format!("gives {}", item)); }
        let effect_str = if effects.is_empty() { "—".to_string() } else { effects.join(", ") };
        let cost_str = if svc.gold_cost > 0 { format!("{}g", svc.gold_cost) } else { "FREE".to_string() };
        manual += &format!("| {} | {} | {} | {} ticks | {} |\n",
            svc.action_name, svc.building_name, cost_str, svc.duration_ticks, effect_str);
    }
    manual += "\n";

    // Append the MCP-generated action reference (or a fallback).
    if let Some(mcp_section) = mcp_manual {
        tracing::info!("[MANUAL] Generated from mcp-game ({} bytes)", mcp_section.len());
        manual += &mcp_section;
    } else {
        tracing::warn!("[MANUAL] mcp-game --generate-manual failed, using fallback");
        manual += "## Actions\nUse the game_action MCP tool. Run inspect service='game_manual' for the latest reference.\n";
    }

    manual
}
