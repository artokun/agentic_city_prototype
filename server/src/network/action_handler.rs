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
    pub x: Option<i32>,
    pub y: Option<i32>,
}

/// System: apply pending MCP actions to agent entities.
pub fn apply_mcp_actions_system(
    mut commands: Commands,
    mut pending: ResMut<PendingActions>,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    mut event_log: ResMut<AgentEventLog>,
    mut suggestion_box: ResMut<SuggestionBox>,
    mut bounty_registry: ResMut<BountyRegistry>,
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

        // Helper: find building and get location info for error messages.
        let find_building = |name: &str| -> Option<(Entity, GridPos, String)> {
            structure_list.iter().find(|(_, _, s)| s == name).map(|(e, p, s)| (*e, *p, s.clone()))
        };

        let known_buildings: Vec<String> = structure_list.iter().map(|(_, _, s)| s.clone()).collect();

        match action_str.as_str() {
            "go_to_board" => {
                *goal = AgentGoal::GoingToBoard;
                if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        thought.0 = format!("Heading to the bounty board ({} tiles).", tiles);
                    } else {
                        thought.0 = "ERROR: Can't find path to the bounty board!".into();
                    }
                } else {
                    thought.0 = "ERROR: Don't know where the bounty board is. Try look_around first.".into();
                }
            }

            "go_to_service" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");
                let service = mcp_action.service.as_deref().unwrap_or("browse");

                if let Some((bld_entity, entrance, _)) = find_building(building) {
                    *goal = AgentGoal::GoingToService {
                        building: bld_entity,
                        service: service.into(),
                    };
                    if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        thought.0 = format!("Walking to {} for {} ({} tiles).", building, service, tiles);
                    } else {
                        thought.0 = format!("ERROR: Can't find path to {}. Try look_around.", building);
                    }
                } else {
                    thought.0 = format!(
                        "ERROR: Unknown building '{}'. Known buildings: {}. Try look_around to discover more.",
                        building, known_buildings.join(", "),
                    );
                }
            }

            "look_around" => {
                commands.entity(entity).insert(WantsToLook);
                thought.0 = "Scanning surroundings...".into();
            }

            "wander" => {
                let walkable = map.walkable_positions();
                let idx = (entity.to_bits() as usize + tick.0 as usize) % walkable.len();
                if let Some(p) = pathfinding::bfs(&map, *pos, walkable[idx]) {
                    let tiles = p.len();
                    if tiles > 0 {
                        commands.entity(entity).insert(Path(p));
                        *goal = AgentGoal::Wandering;
                        thought.0 = format!("Wandering ({} tiles).", tiles);
                    }
                }
            }

            "work_shift" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");

                if let Some((bld_entity, entrance, _)) = find_building(building) {
                    // Check if agent is already at the building.
                    let at_building = pos.x == entrance.x && pos.y == entrance.y;
                    if at_building {
                        // Start shift immediately.
                        *goal = AgentGoal::GoingToService {
                            building: bld_entity,
                            service: "work_shift".into(),
                        };
                        thought.0 = format!("Starting shift at {}.", building);
                    } else {
                        // Walk there first.
                        *goal = AgentGoal::GoingToService {
                            building: bld_entity,
                            service: "work_shift".into(),
                        };
                        if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                            let tiles = p.len();
                            commands.entity(entity).insert(Path(p));
                            thought.0 = format!(
                                "Walking to {} to work a shift ({} tiles). You must be at the building to start working.",
                                building, tiles,
                            );
                        } else {
                            thought.0 = format!("ERROR: Can't find path to {}.", building);
                        }
                    }
                } else {
                    thought.0 = format!(
                        "ERROR: Unknown building '{}'. Shiftable buildings: cafe, market, warehouse, hotel. Use go_to_service to walk there first.",
                        building,
                    );
                }
            }

            "leave_shift" => {
                if matches!(*goal, AgentGoal::WorkingShift { .. }) {
                    *goal = AgentGoal::Idle;
                    thought.0 = "Left the shift. Paycheck earned based on ticks worked.".into();
                } else {
                    thought.0 = "ERROR: Not currently working a shift. Nothing to leave.".into();
                }
            }

            "complete_bounty" => {
                if let AgentGoal::ExecutingBounty(bounty_id) = *goal {
                    *goal = AgentGoal::ReturningToBoard(bounty_id);
                    if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                        if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                            let tiles = p.len();
                            commands.entity(entity).insert(Path(p));
                            thought.0 = format!("Bounty complete! Returning to board ({} tiles).", tiles);
                        }
                    }
                } else {
                    thought.0 = "ERROR: No active bounty to complete. Claim one at the bounty board first.".into();
                }
            }

            "go_to" => {
                if let (Some(x), Some(y)) = (mcp_action.x, mcp_action.y) {
                    let target = GridPos { x, y };
                    if !map.is_walkable(&target) {
                        thought.0 = format!("ERROR: ({},{}) is not walkable (might be inside a building). Try nearby coordinates.", x, y);
                    } else if let Some(p) = pathfinding::bfs(&map, *pos, target) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        *goal = AgentGoal::Wandering;
                        thought.0 = format!("Walking to ({},{}) — {} tiles.", x, y, tiles);
                    } else {
                        thought.0 = format!("ERROR: Can't find path to ({},{}). The map is 40x40.", x, y);
                    }
                } else {
                    thought.0 = "ERROR: go_to requires x and y coordinates. Map is 40x40 (0-39).".into();
                }
            }

            "chat_with" => {
                let target = mcp_action.agent_target.as_deref().unwrap_or("someone");
                *goal = AgentGoal::Wandering;
                thought.0 = format!("Looking to chat with {}...", target);
            }

            "send_message" => {
                if let (Some(recipient), Some(text)) = (&mcp_action.agent_target, &mcp_action.text) {
                    commands.entity(entity).insert(WantsToSendMessage {
                        recipient_name: recipient.clone(),
                        text: text.clone(),
                    });
                    thought.0 = format!("Sent message to {}.", recipient);
                } else {
                    thought.0 = "ERROR: send_message requires 'agent' and 'text' parameters.".into();
                }
            }

            "claim_bounty" => {
                if !matches!(*goal, AgentGoal::InteractingWithBoard) {
                    thought.0 = "Must be at the bounty board to claim a bounty! Go there first.".into();
                } else {
                    let keyword = mcp_action.text.as_deref()
                        .or(mcp_action.service.as_deref())
                        .unwrap_or("");

                    // Find first available bounty matching keyword.
                    let bounty_match = bounty_registry.available().iter()
                        .find(|b| keyword.is_empty() || b.description.to_lowercase().contains(&keyword.to_lowercase()))
                        .map(|b| b.id);

                    if let Some(bounty_id) = bounty_match {
                        if let Some(bounty) = bounty_registry.claim(bounty_id, entity, tick.0) {
                            let desc = bounty.description.clone();
                            let claim_items = bounty.claim_items.clone();

                            // Give claim items to agent.
                            // Need mutable inventory — but we only have the goal query.
                            // We'll handle inventory via a deferred command.
                            thought.0 = format!("Claimed: {}", desc);

                            event_log.push(LogEvent {
                                tick: tick.0,
                                agent: name.0.clone(),
                                kind: LogKind::Action,
                                text: format!("CLAIMED: {} ({}g)", desc, bounty.reward_gold),
                            });

                            tracing::info!("[MCP:{}] claimed bounty: {}", name.0, desc);

                            // Leave the board queue.
                            *goal = AgentGoal::ExecutingBounty(bounty_id);
                        } else {
                            thought.0 = "Couldn't claim that bounty — it may already be taken.".into();
                        }
                    } else {
                        thought.0 = format!("No bounty matching '{}' found.", keyword);
                    }
                }
            }

            "leave_board" => {
                *goal = AgentGoal::Idle;
                thought.0 = "Left the bounty board.".into();
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
