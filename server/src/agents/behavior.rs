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
        needs, path, action_timer, inside,
    ) in &mut agents {
        // Busy with timed action — skip.
        if action_timer.is_some() {
            if !matches!(*goal, AgentGoal::PerformingAction) {
                *goal = AgentGoal::PerformingAction;
            }
            continue;
        }

        // Just finished an action — go back to Idle for AI to decide next.
        if *goal == AgentGoal::PerformingAction {
            if inside.is_some() {
                commands.entity(agent_entity).remove::<InsideBuilding>();
            }
            *goal = AgentGoal::Idle;
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
                            let svc = services::all_services().into_iter().find(|s| s.action_name == service);
                            if let Some(svc) = svc {
                                thought.0 = format!("{}...", svc.action_name);
                                commands.entity(agent_entity).insert(InsideBuilding(building));
                                commands.entity(agent_entity).insert(ActionTimer {
                                    action_name: svc.action_name.to_string(),
                                    remaining_ticks: svc.duration_ticks,
                                    effects: svc.effects,
                                    gold_cost: svc.gold_cost,
                                    paid: false,
                                    consumes_item: svc.consumes_item,
                                });
                                *goal = AgentGoal::PerformingAction;
                            } else { *goal = AgentGoal::Idle; }
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
                    if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                        if queue.try_interact(agent_entity) {
                            thought.0 = "Browsing the bounty board...".into();
                            anim.0 = AnimState::Working;
                            *goal = AgentGoal::InteractingWithBoard;
                        } else {
                            queue.join_queue(agent_entity);
                            thought.0 = "Waiting in line...".into();
                            *goal = AgentGoal::WaitingAtBoard;
                        }
                    }
                } else if !has_path {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                        commands.entity(agent_entity).insert(Path(p));
                    }
                }
            }

            AgentGoal::WaitingAtBoard => {
                // Leave queue if critical need.
                if let Some(need) = needs.most_urgent(CRITICAL_THRESHOLD) {
                    if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                        queue.leave(agent_entity);
                    }
                    thought.0 = format!("Can't wait, {:?} critical!", need);
                    *goal = AgentGoal::Idle;
                    continue;
                }
                if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                    if queue.try_interact(agent_entity) {
                        thought.0 = "My turn! Browsing bounties...".into();
                        anim.0 = AnimState::Working;
                        *goal = AgentGoal::InteractingWithBoard;
                    }
                }
            }

            AgentGoal::InteractingWithBoard => {
                let available: Vec<_> = bounty_registry.available().iter()
                    .map(|b| (b.id, b.description.clone())).collect();

                if let Some((bounty_id, desc)) = available.first() {
                    let bounty_id = *bounty_id;
                    if let Some(bounty) = bounty_registry.claim(bounty_id, agent_entity, tick.0) {
                        let claim_items = bounty.claim_items.clone();
                        thought.0 = format!("Claimed: {desc}");
                        tracing::info!("{} claimed: {desc}", name.0);
                        for (item, count) in &claim_items { inv.add(*item, *count); }
                        if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                            queue.leave(agent_entity);
                        }
                        anim.0 = AnimState::Idle;
                        *goal = AgentGoal::ExecutingBounty(bounty_id);
                    }
                } else {
                    thought.0 = "No bounties available.".into();
                    if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                        queue.leave(agent_entity);
                    }
                    anim.0 = AnimState::Idle;
                    *goal = AgentGoal::Idle; // Back to idle — AI decides next.
                }
            }

            AgentGoal::ExecutingBounty(bounty_id) => {
                let bounty = bounty_registry.get(bounty_id).cloned();
                let Some(bounty) = bounty else { *goal = AgentGoal::Idle; continue };

                match bounty.objective {
                    BountyObjective::HideItem(item) => {
                        if !has_path {
                            if inv.has(item, 1) {
                                let candidates: Vec<_> = structure_list.iter()
                                    .filter(|(_, _, s)| s != "bounty_board").collect();
                                if let Some((se, entrance, sname)) = candidates.first() {
                                    if at_pos(pos, entrance) {
                                        commands.entity(agent_entity).insert(InsideBuilding(*se));
                                        if inv.remove(item, 1) {
                                            if let Ok((_, _, _, mut sinv, _)) = structures.get_mut(*se) {
                                                sinv.add(item, 1);
                                                thought.0 = format!("Hid {} in {}!", item, sname);
                                                tracing::info!("{} hid {} in {}", name.0, item, sname);
                                                bounty_registry.mark_completed(bounty_id);
                                                commands.entity(agent_entity).remove::<InsideBuilding>();
                                                *goal = AgentGoal::ReturningToBoard(bounty_id);
                                                if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                                                    commands.entity(agent_entity).insert(Path(p));
                                                }
                                            }
                                        }
                                    } else {
                                        thought.0 = format!("→ {} to hide {}...", sname, item);
                                        if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                            commands.entity(agent_entity).insert(Path(p));
                                        }
                                    }
                                }
                            } else { *goal = AgentGoal::Idle; }
                        }
                    }
                    BountyObjective::FindItem(item) => {
                        if !has_path {
                            if inv.has(item, 1) {
                                thought.0 = format!("Found {}! Returning...", item);
                                commands.entity(agent_entity).remove::<InsideBuilding>();
                                bounty_registry.mark_completed(bounty_id);
                                *goal = AgentGoal::ReturningToBoard(bounty_id);
                                if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                                    commands.entity(agent_entity).insert(Path(p));
                                }
                                continue;
                            }
                            let mut found_at = None;
                            for (se, entrance, sname) in &structure_list {
                                if sname == "bounty_board" { continue; }
                                if let Ok((_, _, _, sinv, _)) = structures.get(*se) {
                                    if sinv.has(item, 1) {
                                        found_at = Some((*se, *entrance, sname.clone()));
                                        break;
                                    }
                                }
                            }
                            if let Some((se, entrance, sname)) = found_at {
                                if at_pos(pos, &entrance) {
                                    commands.entity(agent_entity).insert(InsideBuilding(se));
                                    if let Ok((_, _, _, mut sinv, _)) = structures.get_mut(se) {
                                        if sinv.remove(item, 1) {
                                            inv.add(item, 1);
                                            thought.0 = format!("Found {} in {}!", item, sname);
                                            tracing::info!("{} found {} in {}", name.0, item, sname);
                                        }
                                    }
                                } else {
                                    thought.0 = format!("Searching {}...", sname);
                                    if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                                        commands.entity(agent_entity).insert(Path(p));
                                    }
                                }
                            } else {
                                let candidates: Vec<_> = structure_list.iter()
                                    .filter(|(_, _, s)| s != "bounty_board").collect();
                                let idx = (agent_entity.to_bits() as usize + tick.0 as usize) % candidates.len().max(1);
                                if let Some((_, entrance, sname)) = candidates.get(idx) {
                                    thought.0 = format!("Searching {}...", sname);
                                    if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                        commands.entity(agent_entity).insert(Path(p));
                                    }
                                }
                            }
                        }
                    }
                    BountyObjective::RestockDelivery { item, quantity, ref destination } => {
                        if !has_path {
                            if !inv.has(item, quantity) {
                                let warehouse = structure_list.iter().find(|(_, _, s)| s == "warehouse");
                                if let Some((_we, w_entrance, _)) = warehouse {
                                    if at_pos(pos, w_entrance) {
                                        let (gold_per, units_per) = item.wholesale_price().unwrap_or((1, 1));
                                        let batches = (quantity + units_per - 1) / units_per;
                                        let cost = batches * gold_per;
                                        if inv.has(ItemType::GoldCoin, cost) {
                                            inv.remove(ItemType::GoldCoin, cost);
                                            inv.add(item, quantity);
                                            thought.0 = format!("Bought {} {} for {}g", quantity, item, cost);
                                            tracing::info!("{} bought {} {} for {}g", name.0, quantity, item, cost);
                                        } else {
                                            thought.0 = format!("Can't afford ({}g needed)", cost);
                                            *goal = AgentGoal::Idle;
                                            continue;
                                        }
                                    } else {
                                        thought.0 = format!("→ warehouse to buy {} {}...", quantity, item);
                                        if let Some(p) = pathfinding::bfs(&map, *pos, *w_entrance) {
                                            commands.entity(agent_entity).insert(Path(p));
                                        }
                                        continue;
                                    }
                                }
                            }
                            if inv.has(item, quantity) {
                                let dest = structure_list.iter().find(|(_, _, s)| s == destination);
                                if let Some((de, d_entrance, dname)) = dest {
                                    if at_pos(pos, d_entrance) {
                                        if inv.remove(item, quantity) {
                                            if let Ok((_, _, _, mut sinv, _)) = structures.get_mut(*de) {
                                                sinv.add(item, quantity);
                                            }
                                            thought.0 = format!("Delivered {} {} to {}!", quantity, item, dname);
                                            tracing::info!("{} delivered {} {} to {}", name.0, quantity, item, dname);
                                            bounty_registry.mark_completed(bounty_id);
                                            *goal = AgentGoal::ReturningToBoard(bounty_id);
                                            if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                                                commands.entity(agent_entity).insert(Path(p));
                                            }
                                        }
                                    } else {
                                        thought.0 = format!("Delivering to {}...", dname);
                                        if let Some(p) = pathfinding::bfs(&map, *pos, *d_entrance) {
                                            commands.entity(agent_entity).insert(Path(p));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    BountyObjective::WorkAtBuilding => {
                        if !has_path {
                            let target = structure_list.iter().find(|(_, _, sname)| {
                                bounty.description.contains(sname.as_str())
                            });
                            if let Some((se, entrance, sname)) = target {
                                if at_pos(pos, entrance) {
                                    commands.entity(agent_entity).insert(InsideBuilding(*se));
                                    commands.entity(agent_entity).insert(ActionTimer {
                                        action_name: format!("working: {}", bounty.description),
                                        remaining_ticks: 40,
                                        effects: services::ServiceEffects { boredom: 15.0, ..Default::default() },
                                        gold_cost: 0,
                                        paid: true,
                                        consumes_item: None,
                                    });
                                    thought.0 = format!("Working: {}...", bounty.description);
                                    tracing::info!("{} working: {}", name.0, bounty.description);
                                    bounty_registry.mark_completed(bounty_id);
                                    *goal = AgentGoal::ReturningToBoard(bounty_id);
                                } else {
                                    thought.0 = format!("→ {} for work...", sname);
                                    if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                        commands.entity(agent_entity).insert(Path(p));
                                    }
                                }
                            } else {
                                bounty_registry.mark_completed(bounty_id);
                                *goal = AgentGoal::ReturningToBoard(bounty_id);
                                if let Some(p) = pathfinding::bfs(&map, *pos, board_entrance) {
                                    commands.entity(agent_entity).insert(Path(p));
                                }
                            }
                        }
                    }
                }
            }

            AgentGoal::ReturningToBoard(bounty_id) => {
                if !has_path && at_pos(pos, &board_entrance) {
                    if let Ok((_, _, mut queue)) = boards.get_mut(board_entity) {
                        if queue.try_interact(agent_entity) {
                            if let Some(bounty) = bounty_registry.get(bounty_id).cloned() {
                                let can_claim = match bounty.objective {
                                    BountyObjective::FindItem(item) => inv.has(item, 1),
                                    _ => true,
                                };
                                if can_claim {
                                    if let BountyObjective::FindItem(item) = bounty.objective {
                                        inv.remove(item, 1);
                                    }
                                    inv.add(ItemType::GoldCoin, bounty.reward_gold);
                                    thought.0 = format!("Collected {} gold!", bounty.reward_gold);
                                    tracing::info!("{} +{} gold", name.0, bounty.reward_gold);
                                }
                            }
                            queue.leave(agent_entity);
                            anim.0 = AnimState::Idle;
                            *goal = AgentGoal::Idle;
                        } else {
                            queue.join_queue(agent_entity);
                            thought.0 = "Waiting to claim reward...".into();
                        }
                    }
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
