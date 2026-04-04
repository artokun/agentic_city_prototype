use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agents::ai_decision::{self, AgentAction};
use crate::agents::claude;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::perception::KnownLocations;
use crate::agents::social::Relationships;
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::BountyRegistry;
use crate::world::map::{GridPos, WorldMap};
use crate::world::structures::{Entrance, SpriteType, StructureId};

use super::actions::ActionTimer;
use super::pathfinding;
use super::personality::Personality;

/// Bevy resource: tracks pending AI decisions per agent.
#[derive(Resource, Default)]
pub struct AgentSessions {
    pub sessions: std::collections::HashMap<Entity, SessionState>,
}

pub struct SessionState {
    pub last_decision_tick: u64,
    /// If Some, a Claude response is being awaited.
    pub pending_response: Option<Arc<Mutex<Option<String>>>>,
    pub system_prompt: String,
}

const DECISION_INTERVAL: u64 = 50; // 5 seconds between decisions

/// System: initialize session state for agents that don't have one.
pub fn spawn_sessions_system(
    mut sessions: ResMut<AgentSessions>,
    agents: Query<(Entity, &AgentName, &Personality)>,
) {
    for (entity, name, personality) in &agents {
        if sessions.sessions.contains_key(&entity) {
            continue;
        }
        let system_prompt = super::personality::build_system_prompt(&name.0, personality);
        sessions.sessions.insert(entity, SessionState {
            last_decision_tick: 0,
            pending_response: None,
            system_prompt,
        });
    }
}

/// System: send decision prompts to Claude and apply responses.
pub fn ai_decision_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    bounty_registry: Res<BountyRegistry>,
    runtime: ResMut<TokioTasksRuntime>,
    mut sessions: ResMut<AgentSessions>,
    mut event_log: ResMut<AgentEventLog>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos, &Speed,
        &mut AgentGoal, &mut ThoughtBubble,
        &Needs, &Inventory, &KnownLocations, &Relationships,
        Option<&ActionTimer>, Option<&Path>,
    )>,
    all_agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter().map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone())).collect();

    for (
        entity, name, pos, speed, mut goal, mut thought,
        needs, inv, known_locs, rels,
        action_timer, path,
    ) in &mut agents {
        if action_timer.is_some() { continue; }
        let has_path = path.is_some_and(|p| !p.0.is_empty());
        if has_path { continue; }

        let Some(session) = sessions.sessions.get_mut(&entity) else { continue };

        // Check for pending response.
        if let Some(ref response_slot) = session.pending_response {
            let maybe_response = response_slot.lock().unwrap().take();
            if let Some(response_text) = maybe_response {
                session.pending_response = None;
                let (action, thought_text) = ai_decision::parse_action(&response_text);
                thought.0 = thought_text.clone();
                tracing::info!("[AI:{}] {} → {:?}", name.0, thought_text, action);

                event_log.push(LogEvent {
                    tick: tick.0, agent: name.0.clone(),
                    kind: LogKind::Thought, text: thought_text.clone(),
                });
                event_log.push(LogEvent {
                    tick: tick.0, agent: name.0.clone(),
                    kind: LogKind::Decision, text: format!("{:?}", action),
                });

                match &action {
                    AgentAction::WorkShift { building } => {
                        if let Some((bld_entity, entrance, _)) = structure_list.iter().find(|(_, _, s)| s == building) {
                            *goal = AgentGoal::GoingToService {
                                building: *bld_entity,
                                service: "work_shift".into(),
                            };
                            if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                commands.entity(entity).insert(Path(p));
                            }
                        }
                    }
                    AgentAction::LeaveShift => {
                        *goal = AgentGoal::Idle;
                    }
                    _ => {
                        apply_action(&mut commands, entity, &action, &mut goal, pos, &map, &structure_list, known_locs);
                    }
                }
            }
            continue; // Still waiting for response.
        }

        // Rate limit.
        if tick.0 - session.last_decision_tick < DECISION_INTERVAL { continue; }
        if !matches!(*goal, AgentGoal::Idle | AgentGoal::Wandering) { continue; }

        // Build context.
        let available_bounties: Vec<String> = bounty_registry
            .available().iter()
            .map(|b| format!("{} ({}g)", b.description, b.reward_gold)).collect();

        let nearby: Vec<(String, GridPos)> = all_agents.iter()
            .filter(|(n, _)| n.0 != name.0)
            .filter(|(_, p)| (p.x - pos.x).abs() + (p.y - pos.y).abs() <= 10)
            .map(|(n, p)| (n.0.clone(), *p)).collect();

        let location_tools: Vec<&str> = vec![];

        let context = ai_decision::build_context(
            &name.0, pos, needs, inv, &goal, known_locs, rels,
            speed.0, &available_bounties, &nearby, &location_tools,
        );

        // Spawn async Claude call.
        let response_slot = Arc::new(Mutex::new(None::<String>));
        let slot_clone = response_slot.clone();
        let system_prompt = session.system_prompt.clone();
        let agent_name = name.0.clone();

        runtime.spawn_background_task(move |_ctx| async move {
            match claude::ask_claude(&context, &system_prompt).await {
                Ok(response) => {
                    tracing::info!("[Claude:{}] response: {}", agent_name, &response[..response.len().min(100)]);
                    *slot_clone.lock().unwrap() = Some(response);
                }
                Err(e) => {
                    tracing::error!("[Claude:{}] error: {}", agent_name, e);
                    *slot_clone.lock().unwrap() = Some(r#"{"action": "wander", "thought": "Claude error, wandering"}"#.to_string());
                }
            }
        });

        session.pending_response = Some(response_slot);
        session.last_decision_tick = tick.0;
    }
}

fn apply_action(
    commands: &mut Commands, entity: Entity, action: &AgentAction,
    goal: &mut Mut<AgentGoal>, pos: &GridPos, map: &WorldMap,
    structures: &[(Entity, GridPos, String)], known_locs: &KnownLocations,
) {
    match action {
        AgentAction::GoToBoard => {
            **goal = AgentGoal::GoingToBoard;
            if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                if let Some(p) = pathfinding::bfs(map, *pos, board.entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::GoToService { building, service } => {
            if let Some((bld_entity, entrance, _)) = structures.iter().find(|(_, _, s)| s == building) {
                **goal = AgentGoal::GoingToService { building: *bld_entity, service: service.clone() };
                if let Some(p) = pathfinding::bfs(map, *pos, *entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::LookAround => {
            commands.entity(entity).insert(super::perception::WantsToLook);
        }
        AgentAction::Wander => {
            let walkable = map.walkable_positions();
            let idx = (entity.to_bits() as usize + 7) % walkable.len();
            if let Some(p) = pathfinding::bfs(map, *pos, walkable[idx]) {
                if !p.is_empty() {
                    commands.entity(entity).insert(Path(p));
                    **goal = AgentGoal::Wandering;
                }
            }
        }
        AgentAction::ChatWith { .. } => { **goal = AgentGoal::Wandering; }
        AgentAction::WorkShift { .. } | AgentAction::LeaveShift => {}
        AgentAction::DoNothing => {}
    }
}
