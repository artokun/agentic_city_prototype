use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agents::ai_decision::{self, AgentAction};
use crate::agents::claude;
use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::agents::perception::{KnownLocations, Vision, WantsToLook};
use crate::agents::social::Relationships;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::BountyRegistry;
use crate::world::map::{GridPos, WorldMap};
use crate::world::services;
use crate::world::structures::{Entrance, SpriteType, StructureId};

use super::actions::ActionTimer;
use super::pathfinding;
use super::personality::Personality;

/// Bevy resource: holds the channel senders for each agent's Claude session.
#[derive(Resource, Default)]
pub struct AgentSessions {
    pub sessions: std::collections::HashMap<Entity, AgentSessionHandle>,
}

pub struct AgentSessionHandle {
    pub prompt_tx: mpsc::Sender<String>,
    pub response_rx: Arc<Mutex<mpsc::Receiver<serde_json::Value>>>,
    pub last_decision_tick: u64,
    pub pending: bool,
}

/// How often agents ask Claude for decisions (in ticks). At 10Hz, 30 ticks = 3 seconds.
const DECISION_INTERVAL: u64 = 30;

/// System: spawn Claude sessions for agents that don't have one.
pub fn spawn_sessions_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut sessions: ResMut<AgentSessions>,
    agents: Query<(Entity, &AgentName, &Personality), Without<ActionTimer>>,
) {
    for (entity, name, personality) in &agents {
        if sessions.sessions.contains_key(&entity) {
            continue;
        }

        let agent_name = name.0.clone();
        let system_prompt = super::personality::build_system_prompt(&name.0, personality);

        tracing::info!("Spawning Claude session for {}", agent_name);

        let (prompt_tx, prompt_rx_inner) = mpsc::channel::<String>(16);
        let (response_tx, response_rx) = mpsc::channel::<serde_json::Value>(64);

        sessions.sessions.insert(entity, AgentSessionHandle {
            prompt_tx: prompt_tx.clone(),
            response_rx: Arc::new(Mutex::new(response_rx)),
            last_decision_tick: 0,
            pending: false,
        });

        // Spawn the Claude process in a background task.
        let prompt_tx_clone = prompt_tx.clone();
        runtime.spawn_background_task(move |_ctx| async move {
            match claude::spawn_claude_session(&agent_name, &system_prompt).await {
                Ok(mut session) => {
                    // Bridge: forward prompts from game → Claude, responses from Claude → game.
                    let mut prompt_rx = prompt_rx_inner;

                    // Forward prompts.
                    let stx = session.prompt_tx.clone();
                    tokio::spawn(async move {
                        while let Some(prompt) = prompt_rx.recv().await {
                            let _ = stx.send(prompt).await;
                        }
                    });

                    // Forward responses.
                    while let Some(msg) = session.response_rx.recv().await {
                        if response_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to spawn Claude session: {}", e);
                }
            }
        });
    }
}

/// System: send decision prompts to Claude and apply responses.
pub fn ai_decision_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    bounty_registry: Res<BountyRegistry>,
    mut sessions: ResMut<AgentSessions>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &Speed,
        &mut AgentGoal,
        &mut ThoughtBubble,
        &Needs,
        &Inventory,
        &KnownLocations,
        &Relationships,
        &Vision,
        Option<&ActionTimer>,
        Option<&Path>,
    )>,
    all_agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter()
        .map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone()))
        .collect();

    for (
        entity, name, pos, speed, mut goal, mut thought,
        needs, inv, known_locs, rels, _vision,
        action_timer, path,
    ) in &mut agents {
        // Skip if busy.
        if action_timer.is_some() {
            continue;
        }
        let has_path = path.is_some_and(|p| !p.0.is_empty());
        if has_path {
            continue;
        }

        let Some(session) = sessions.sessions.get_mut(&entity) else {
            continue;
        };

        // Check for pending responses.
        if session.pending {
            let response = {
                let mut rx = session.response_rx.lock().unwrap();
                claude::drain_response(&mut rx)
            };

            if let Some(response_text) = response {
                session.pending = false;
                let (action, thought_text) = ai_decision::parse_action(&response_text);
                thought.0 = thought_text.clone();
                tracing::info!("[AI:{}] {} → {:?}", name.0, thought_text, action);

                apply_action(
                    &mut commands, entity, &action, &mut goal,
                    pos, &map, &structure_list, &known_locs,
                );
            }
            continue;
        }

        // Rate limit decisions.
        if tick.0 - session.last_decision_tick < DECISION_INTERVAL {
            continue;
        }

        // Only ask for decisions when idle.
        if !matches!(*goal, AgentGoal::Idle | AgentGoal::Wandering) {
            continue;
        }

        // Build context and send to Claude.
        let available_bounties: Vec<String> = bounty_registry
            .available()
            .iter()
            .map(|b| format!("{} ({}g)", b.description, b.reward_gold))
            .collect();

        let nearby: Vec<(String, GridPos)> = all_agents
            .iter()
            .filter(|(n, _)| n.0 != name.0)
            .filter(|(_, p)| (p.x - pos.x).abs() + (p.y - pos.y).abs() <= 10)
            .map(|(n, p)| (n.0.clone(), *p))
            .collect();

        // TODO: location-specific tools based on current building
        let location_tools: Vec<&str> = vec![];

        let context = ai_decision::build_context(
            &name.0, pos, needs, inv, &goal, known_locs, rels,
            speed.0, &available_bounties, &nearby, &location_tools,
        );

        if let Err(e) = session.prompt_tx.try_send(context) {
            tracing::warn!("[AI:{}] Failed to send prompt: {}", name.0, e);
            continue;
        }

        session.pending = true;
        session.last_decision_tick = tick.0;
    }
}

fn apply_action(
    commands: &mut Commands,
    entity: Entity,
    action: &AgentAction,
    goal: &mut Mut<AgentGoal>,
    pos: &GridPos,
    map: &WorldMap,
    structures: &[(Entity, GridPos, String)],
    known_locs: &KnownLocations,
) {
    match action {
        AgentAction::GoToBoard => {
            **goal = AgentGoal::GoingToBoard;
            // Find board entrance from known locations.
            if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                if let Some(p) = pathfinding::bfs(map, *pos, board.entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::GoToService { building, service } => {
            // Find the building entity.
            if let Some((bld_entity, entrance, _)) = structures.iter().find(|(_, _, s)| s == building) {
                **goal = AgentGoal::GoingToService {
                    building: *bld_entity,
                    service: service.clone(),
                };
                if let Some(p) = pathfinding::bfs(map, *pos, *entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::LookAround => {
            commands.entity(entity).insert(WantsToLook);
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
        AgentAction::ChatWith { agent: _ } => {
            // Social matchmaking handles this — just wander toward nearby agents.
            **goal = AgentGoal::Wandering;
        }
        AgentAction::DoNothing => {}
    }
}
