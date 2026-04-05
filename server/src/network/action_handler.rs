//! Processes MCP game_action commands by applying them to agent ECS entities.
//! This is the authoritative bridge between Claude's tool calls and the game world.

use bevy::prelude::*;
use tokio::sync::mpsc;

use crate::agents::ai_decision::AgentAction;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::mailbox::WantsToSendMessage;
use crate::agents::pathfinding;
use crate::agents::perception::{KnownLocations, WantsToLook};
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::BountyRegistry;
use crate::world::map::{GridPos, WorldMap};
use crate::world::shifts::ShiftWorker;
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

/// Pending action from an MCP tool call, waiting to be applied.
#[derive(Resource, Default)]
pub struct PendingActions {
    pub actions: Vec<MpcAction>,
}

/// Agent feedback/bug reports/suggestions — visible in debug monitor.
#[derive(Resource, Default)]
pub struct SuggestionBox {
    pub entries: Vec<Suggestion>,
}

pub struct Suggestion {
    pub tick: u64,
    pub agent: String,
    pub text: String,
}

pub struct MpcAction {
    pub agent_name: String,
    pub agent_id: String,
    pub action: String,
    pub building: Option<String>,
    pub service: Option<String>,
    pub agent_target: Option<String>,
    pub text: Option<String>,
    pub feedback: Option<String>,
}

/// System: apply pending MCP actions to agent entities.
pub fn apply_mcp_actions_system(
    mut commands: Commands,
    mut pending: ResMut<PendingActions>,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    mut event_log: ResMut<AgentEventLog>,
    mut suggestion_box: ResMut<SuggestionBox>,
    bounty_registry: Res<BountyRegistry>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos,
        &mut AgentGoal, &mut ThoughtBubble,
        &KnownLocations, Option<&ShiftWorker>,
    )>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter().map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone())).collect();

    for mcp_action in pending.actions.drain(..) {
        // Find the agent entity by name.
        let agent = agents.iter_mut()
            .find(|(_, name, _, _, _, _, _)| name.0 == mcp_action.agent_name);

        let Some((entity, name, pos, mut goal, mut thought, known_locs, shift_worker)) = agent else {
            tracing::warn!("[MCP] Agent '{}' not found", mcp_action.agent_name);
            continue;
        };

        let action_str = &mcp_action.action;
        tracing::info!("[MCP:{}] action={} building={:?} service={:?}",
            name.0, action_str, mcp_action.building, mcp_action.service);

        event_log.push(LogEvent {
            tick: tick.0,
            agent: name.0.clone(),
            kind: LogKind::Action,
            text: format!("→ {} {:?} {:?}", action_str, mcp_action.building, mcp_action.service),
        });

        match action_str.as_str() {
            "go_to_board" => {
                *goal = AgentGoal::GoingToBoard;
                if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                        commands.entity(entity).insert(Path(p));
                    }
                }
                thought.0 = "Heading to the bounty board.".into();
            }

            "go_to_service" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");
                let service = mcp_action.service.as_deref().unwrap_or("browse");

                if let Some((bld_entity, entrance, _)) = structure_list.iter().find(|(_, _, s)| s == building) {
                    *goal = AgentGoal::GoingToService {
                        building: *bld_entity,
                        service: service.into(),
                    };
                    if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                        commands.entity(entity).insert(Path(p));
                        thought.0 = format!("Walking to {} for {}.", building, service);
                    } else {
                        thought.0 = format!("Can't find path to {}.", building);
                    }
                } else {
                    thought.0 = format!("Don't know where '{}' is.", building);
                }
            }

            "look_around" => {
                commands.entity(entity).insert(WantsToLook);
                thought.0 = "Looking around...".into();
            }

            "wander" => {
                let walkable = map.walkable_positions();
                let idx = (entity.to_bits() as usize + tick.0 as usize) % walkable.len();
                if let Some(p) = pathfinding::bfs(&map, *pos, walkable[idx]) {
                    if !p.is_empty() {
                        commands.entity(entity).insert(Path(p));
                        *goal = AgentGoal::Wandering;
                        thought.0 = "Wandering...".into();
                    }
                }
            }

            "work_shift" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");
                if let Some((bld_entity, entrance, _)) = structure_list.iter().find(|(_, _, s)| s == building) {
                    *goal = AgentGoal::GoingToService {
                        building: *bld_entity,
                        service: "work_shift".into(),
                    };
                    if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                        commands.entity(entity).insert(Path(p));
                        thought.0 = format!("Going to {} to work a shift.", building);
                    }
                }
            }

            "leave_shift" => {
                *goal = AgentGoal::Idle;
                thought.0 = "Leaving my shift.".into();
            }

            "complete_bounty" => {
                if let AgentGoal::ExecutingBounty(bounty_id) = *goal {
                    *goal = AgentGoal::ReturningToBoard(bounty_id);
                    if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                        if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                            commands.entity(entity).insert(Path(p));
                        }
                    }
                    thought.0 = "Bounty complete! Returning to board.".into();
                } else {
                    thought.0 = "No active bounty to complete.".into();
                }
            }

            "chat_with" => {
                // Just wander toward nearby agents — social matchmaking handles the rest.
                *goal = AgentGoal::Wandering;
                thought.0 = "Looking to chat...".into();
            }

            "send_message" => {
                if let (Some(recipient), Some(text)) = (&mcp_action.agent_target, &mcp_action.text) {
                    commands.entity(entity).insert(WantsToSendMessage {
                        recipient_name: recipient.clone(),
                        text: text.clone(),
                    });
                    thought.0 = format!("Sending message to {}.", recipient);
                }
            }

            "help" => {
                let feedback = mcp_action.feedback.clone()
                    .or_else(|| mcp_action.text.clone())
                    .unwrap_or_else(|| "general feedback".into());

                suggestion_box.entries.push(Suggestion {
                    tick: tick.0,
                    agent: name.0.clone(),
                    text: feedback.clone(),
                });

                thought.0 = "Submitted feedback to the developers.".into();

                event_log.push(LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: LogKind::System,
                    text: format!("FEEDBACK: {}", feedback),
                });

                tracing::warn!("[FEEDBACK:{}] {}", name.0, feedback);
            }

            _ => {
                thought.0 = format!("Unknown action: {}", action_str);
            }
        }
    }
}
