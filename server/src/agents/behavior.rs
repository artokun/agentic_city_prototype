//! Execution system — handles the MECHANICAL execution of agent goals.
//! Does NOT make decisions about what to do. That's the AI system's job.
//! Only exception: critical needs (<10) are auto-handled as a safety net.

use bevy::prelude::*;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::*;
use crate::world::map::{GridPos, WorldMap};
use crate::world::services;
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

use super::action_log::{ActionEvent, ActionLog};
use super::actions::ActionTimer;
use super::components::*;
use super::needs::{NeedType, Needs};
use super::pathfinding;
use crate::world::shifts::{ShiftWorker, Staffable};

const CRITICAL_THRESHOLD: f32 = 10.0;

fn at_pos(a: &GridPos, b: &GridPos) -> bool {
    a.x == b.x && a.y == b.y
}

fn pick_service_for_need(
    need: NeedType, agent_pos: &GridPos, agent_gold: u32, agent_speed: f32,
    structures: &[(Entity, GridPos, String)], map: &WorldMap,
) -> Option<(Entity, GridPos, services::BuildingService)> {
    let all = services::all_services();
    let relevant: Vec<_> = all.iter().filter(|s| match need {
        NeedType::Energy => s.effects.energy > 0.0,
        NeedType::Hunger => s.effects.hunger > 0.0,
        NeedType::Boredom => s.effects.boredom > 0.0,
    }).collect();

    let mut best: Option<(Entity, GridPos, &services::BuildingService, f32)> = None;
    for service in &relevant {
        if service.gold_cost > agent_gold { continue; }
        for (entity, entrance, name) in structures {
            if name != service.building_name { continue; }
            let distance = pathfinding::bfs(map, *agent_pos, *entrance)
                .map(|p| p.len() as f32).unwrap_or(999.0);
            let total = distance / agent_speed + service.duration_ticks as f32;
            let effect = match need {
                NeedType::Energy => service.effects.energy,
                NeedType::Hunger => service.effects.hunger,
                NeedType::Boredom => service.effects.boredom,
            };
            let efficiency = effect / total;
            if best.is_none() || efficiency > best.as_ref().unwrap().3 {
                best = Some((*entity, *entrance, service, efficiency));
            }
        }
    }
    best.map(|(e, pos, s, _)| (e, pos, s.clone()))
}

/// Execution system: handles mechanical execution of goals.
/// Agents in Idle/Wandering stay there until the AI system gives them a new goal.
pub fn execution_system(
    mut commands: Commands,
    map: Res<WorldMap>,
    tick: Res<TickCount>,
    mut bounty_registry: ResMut<BountyRegistry>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos, &Speed,
        &mut AgentGoal, &mut ThoughtBubble, &mut AgentAnimation, &mut Inventory,
        &Needs, Option<&Path>, Option<&ActionTimer>, Option<&InsideBuilding>,
        &mut ActionLog,
    )>,
    mut boards: Query<(Entity, &Entrance, &mut BoardQueue), With<BountyBoard>>,
    mut structures: Query<
        (Entity, &Entrance, &SpriteType, &mut Inventory, Option<&mut Staffable>),
        (With<StructureId>, Without<AgentName>),
    >,
) {
    let board_info: Vec<(Entity, GridPos)> = boards.iter().map(|(e, ent, _)| (e, ent.0)).collect();
    let Some((board_entity, board_entrance)) = board_info.first().copied() else { return };

    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter().map(|(e, ent, sprite, _, _)| (e, ent.0, sprite.0.clone())).collect();

    for (
        agent_entity, name, pos, speed, mut goal, mut thought, mut anim, mut inv,
        needs, path, action_timer, inside, mut action_log,
    ) in &mut agents {
        // Busy with timed action — skip.
        if action_timer.is_some() {
            if !matches!(*goal, AgentGoal::PerformingAction) {
                *goal = AgentGoal::PerformingAction;
            }
            continue;
        }

        // Just finished an action — restore previous goal or go Idle.
        if *goal == AgentGoal::PerformingAction {
            if inside.is_some() {
                commands.entity(agent_entity).remove::<InsideBuilding>();
            }

            // Check if agent has an active bounty — restore ExecutingBounty.
            let active_bounty = bounty_registry.bounties.iter()
                .find(|b| b.claimed_by == Some(agent_entity) && b.state == crate::world::bounty::BountyState::Claimed)
                .map(|b| b.id);

            if let Some(bounty_id) = active_bounty {
                *goal = AgentGoal::ExecutingBounty(bounty_id);
                continue;
            }
            *goal = AgentGoal::Idle;
        }

        // Auto-leave board queue if agent is no longer at the board.
        if !matches!(*goal, AgentGoal::InteractingWithBoard | AgentGoal::WaitingAtBoard) {
            if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                if queue.interacting == Some(agent_entity) || queue.waiting.contains(&agent_entity) {
                    queue.leave(agent_entity);
                }
            }
        }

        let has_path = path.is_some_and(|p| !p.0.is_empty());
        let gold = inv.count(ItemType::GoldCoin);
        let current_goal = goal.clone();

        match current_goal {
            // ========== IDLE / WANDERING ==========
            // AI system handles decisions. Only intervene for critical needs.
            AgentGoal::Idle => {
                if let Some(need) = needs.most_urgent(CRITICAL_THRESHOLD) {
                    if let Some((bld, entrance, service)) = pick_service_for_need(
                        need, pos, gold, speed.0, &structure_list, &map,
                    ) {
                        thought.0 = format!("CRITICAL {:?}! → {}", need, service.action_name);
                        *goal = AgentGoal::GoingToService { building: bld, service: service.action_name.into() };
                        if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                            commands.entity(agent_entity).insert(Path(p));
                        }
                    }
                }
                // Otherwise: wait for AI decision. Don't do anything.
            }

            AgentGoal::Wandering => {
                if !has_path { *goal = AgentGoal::Idle; }
            }

            // ========== MECHANICAL EXECUTION ==========
            // Everything below is pure execution — the AI set the goal, we carry it out.

            AgentGoal::GoingToService { building, service } => {
                if !has_path {
                    let at_entrance = structure_list.iter()
                        .find(|(e, _, _)| *e == building)
                        .is_some_and(|(_, ent, _)| at_pos(pos, ent));

                    if at_entrance {
                        // Special case: starting a work shift.
                        if service == "work_shift" {
                            let building_name = structure_list.iter()
                                .find(|(e, _, _)| *e == building)
                                .map(|(_, _, s)| s.clone())
                                .unwrap_or_default();
                            // Read ticks_per_gold from Staffable and mark building as staffed.
                            let ticks_per_gold = if let Ok((_, _, _, _, Some(mut staffable))) = structures.get_mut(building) {
                                staffable.worker = Some(agent_entity);
                                staffable.needs_worker = false;
                                staffable.ticks_per_gold
                            } else {
                                100 // fallback if building has no Staffable
                            };
                            commands.entity(agent_entity).insert(InsideBuilding(building));
                            action_log.log(tick.0, ActionEvent::EnteredBuilding { building: building_name.clone() });
                            commands.entity(agent_entity).insert(ShiftWorker {
                                building,
                                building_name: building_name.clone(),
                                ticks_worked: 0,
                                ticks_per_gold,
                            });
                            thought.0 = format!("Starting shift at {}...", building_name);
                            anim.0 = AnimState::Working;
                            *goal = AgentGoal::WorkingShift { building };
                        } else {
                            let svc_building_name = structure_list.iter()
                                .find(|(e, _, _)| *e == building)
                                .map(|(_, _, s)| s.clone())
                                .unwrap_or_default();

                            // Validate service is available at THIS building.
                            let svc = services::all_services().into_iter().find(|s| {
                                s.action_name == service && s.building_name == svc_building_name
                            });

                            if let Some(svc) = svc {
                                // Special case: search_internet spawns a real research agent.
                                if svc.action_name == "search_internet" {
                                    if inv.has(ItemType::GoldCoin, svc.gold_cost) {
                                        inv.remove(ItemType::GoldCoin, svc.gold_cost);
                                        thought.0 = "Researching... (a research agent is doing real web searches)".into();
                                        commands.entity(agent_entity).insert(InsideBuilding(building));
                                        commands.entity(agent_entity).insert(
                                            super::gm::PendingResearch {
                                                topic: format!("Research for agent {}", name.0),
                                            },
                                        );
                                        *goal = AgentGoal::PerformingAction;
                                    } else {
                                        thought.0 = format!("ERROR: search_internet costs {}g but you only have {}g.", svc.gold_cost, inv.count(ItemType::GoldCoin));
                                        *goal = AgentGoal::Idle;
                                    }
                                } else {
                                    thought.0 = format!("{}...", svc.action_name);
                                    commands.entity(agent_entity).insert(InsideBuilding(building));
                                    action_log.log(tick.0, ActionEvent::EnteredBuilding { building: svc_building_name });
                                    commands.entity(agent_entity).insert(ActionTimer {
                                        action_name: svc.action_name.to_string(),
                                        remaining_ticks: svc.duration_ticks,
                                        effects: svc.effects,
                                        gold_cost: svc.gold_cost,
                                        paid: false,
                                        consumes_item: svc.consumes_item,
                                        produces_item: svc.produces_item,
                                    });
                                    *goal = AgentGoal::PerformingAction;
                                }
                            } else {
                                // Service not available at this building!
                                thought.0 = format!(
                                    "ERROR: '{}' is not available at {}. Check the service list for this building.",
                                    service, svc_building_name,
                                );
                                *goal = AgentGoal::Idle;
                            }
                        }
                    } else {
                        if let Some((_, entrance, _)) = structure_list.iter().find(|(e, _, _)| *e == building) {
                            if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                commands.entity(agent_entity).insert(Path(p));
                            }
                        } else { *goal = AgentGoal::Idle; }
                    }
                }
            }

            AgentGoal::GoingToBoard => {
                if !has_path && at_pos(pos, &board_entrance) {
                    // No queue — multiple agents can browse simultaneously.
                    thought.0 = "Browsing the bounty board...".into();
                    anim.0 = AnimState::Working;
                    *goal = AgentGoal::InteractingWithBoard;
                } else if !has_path {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                        commands.entity(agent_entity).insert(Path(p));
                    }
                }
            }

            AgentGoal::WaitingAtBoard => {
                // Legacy state — immediately transition to interacting.
                thought.0 = "Browsing bounties...".into();
                anim.0 = AnimState::Working;
                *goal = AgentGoal::InteractingWithBoard;
            }

            AgentGoal::InteractingWithBoard => {
                // Agent is at the board. The AI context system will show them
                // available bounties. They must call claim_bounty via MCP tool
                // to pick one. NO auto-claiming.
                // Just wait — the agent is "browsing" and will make a choice.
                // The context system sends bounty details when at_board is true.
                anim.0 = AnimState::Working;
            }

            AgentGoal::ExecutingBounty(bounty_id) => {
                let bounty = bounty_registry.get(bounty_id).cloned();
                let Some(bounty) = bounty else { *goal = AgentGoal::Idle; continue };

                // If the bounty has expired, force return.
                if bounty.expired {
                    thought.0 = "Bounty expired! Must return it...".into();
                    tracing::info!("{} bounty expired, returning to board", name.0);
                    *goal = AgentGoal::ReturningToBoard(bounty_id);
                    if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                        commands.entity(agent_entity).insert(Path(p));
                    }
                    continue;
                }

                // ALL bounty execution is driven by Claude AI.
                // The execution system does NOT decide where to go or what to do.
                // It only handles the expired-bounty safety net above.
                // The AI system will see ExecutingBounty in the agent's goal,
                // include the bounty details in the context, and Claude decides
                // the next action (go_to_service, look_around, etc.)
                //
                // When Claude decides the bounty is complete, it should choose
                // "go_to_board" which transitions to ReturningToBoard.
            }

            AgentGoal::ReturningToBoard(bounty_id) => {
                tracing::debug!("[RETURN] {} goal=ReturningToBoard pos=({},{}) board=({},{}) has_path={} at_board={}",
                    name.0, pos.x, pos.y, board_entrance.x, board_entrance.y, has_path, at_pos(pos, &board_entrance));
                if !has_path && at_pos(pos, &board_entrance) {
                    tracing::info!("[SUBMIT] {} submitting bounty {} for GM verification", name.0, bounty_id);
                    action_log.log(tick.0, ActionEvent::BountyReturned { bounty_id });

                    if let Some(bounty) = bounty_registry.get(bounty_id).cloned() {
                        if bounty.expired {
                            // Expired bounties skip GM — just charge the recycling fee.
                            inv.deduct_gold_with_debt(RECYCLE_COST);
                            thought.0 = format!("Recycled expired bounty (-{}g)", RECYCLE_COST);
                            tracing::info!("{} recycled expired bounty (-{}g)", name.0, RECYCLE_COST);
                            if let Some(b) = bounty_registry.bounties.iter_mut().find(|b| b.id == bounty_id) {
                                b.state = BountyState::Completed;
                            }
                        } else {
                            // Submit for GM verification.
                            thought.0 = "Bounty submitted for Game Master review. Waiting for verdict...".into();
                            if let Some(b) = bounty_registry.bounties.iter_mut().find(|b| b.id == bounty_id) {
                                b.state = BountyState::PendingVerification;
                            }
                            // Spawn GM agent (via marker component).
                            commands.entity(agent_entity).insert(
                                crate::agents::gm::PendingGmReview { bounty_id },
                            );
                        }
                    }
                    anim.0 = AnimState::Idle;
                    *goal = AgentGoal::Idle;
                } else if !has_path {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                        commands.entity(agent_entity).insert(Path(p));
                    }
                }
            }

            AgentGoal::PerformingAction => {} // handled above

            AgentGoal::WorkingShift { .. } => {} // handled by shift_tracking_system
        }
    }
}
