//! Demand-driven staffing system.
//! Buildings need workers to serve customers. Shifts open when a customer
//! arrives and nobody is working. Agents earn gold based on ticks worked.

use bevy::prelude::*;

use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::structures::InsideBuilding;

/// A building that can be staffed.
#[derive(Component, Debug)]
pub struct Staffable {
    /// Ticks of work per 1 gold earned.
    pub ticks_per_gold: u32,
    /// Who is currently staffing this building (if anyone).
    pub worker: Option<Entity>,
    /// Whether a customer is waiting and no worker is present.
    pub needs_worker: bool,
    /// Does working here stop hunger from decaying?
    pub food_perk: bool,
}

/// Attached to an agent who is currently working a shift.
#[derive(Component, Debug)]
pub struct ShiftWorker {
    pub building: Entity,
    pub building_name: String,
    pub ticks_worked: u32,
    pub ticks_per_gold: u32,
}

/// Tracks raw-material processing happening during a shift.
#[derive(Component, Debug)]
pub struct ProcessingTask {
    /// The raw material being consumed.
    pub input: ItemType,
    /// The finished good being produced.
    pub output: ItemType,
    /// Ticks remaining until this unit is done.
    pub ticks_remaining: u32,
}

/// A paycheck item — redeemable at the bounty board.
#[derive(Debug, Clone)]
pub struct Paycheck {
    pub building_name: String,
    pub ticks_worked: u32,
    pub ticks_per_gold: u32,
}

impl Paycheck {
    pub fn gold_value(&self) -> u32 {
        self.ticks_worked / self.ticks_per_gold
    }
}

/// Wallet holding unredeemed paychecks. Agent must visit the bounty board to cash them in.
#[derive(Component, Default, Debug, Clone)]
pub struct PaycheckWallet {
    pub paychecks: Vec<Paycheck>,
}

impl PaycheckWallet {
    pub fn total_gold(&self) -> u32 {
        self.paychecks.iter().map(|p| p.gold_value()).sum()
    }

    pub fn redeem_all(&mut self) -> u32 {
        let total = self.total_gold();
        self.paychecks.clear();
        total
    }
}

/// System: shift workers process raw materials into finished goods.
pub fn shift_processing_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    workers_without_task: Query<
        (Entity, &AgentName, &ShiftWorker),
        Without<ProcessingTask>,
    >,
    mut workers_with_task: Query<
        (Entity, &AgentName, &ShiftWorker, &mut ProcessingTask),
    >,
    mut building_invs: Query<&mut Inventory, Without<AgentName>>,
) {
    // Advance existing processing tasks.
    for (entity, name, shift, mut task) in &mut workers_with_task {
        task.ticks_remaining = task.ticks_remaining.saturating_sub(1);
        if task.ticks_remaining == 0 {
            // Processing complete — add finished good to building inventory.
            if let Ok(mut inv) = building_invs.get_mut(shift.building) {
                inv.add(task.output, 1);
            }

            event_log.push(LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: LogKind::Action,
                text: format!(
                    "Processed {} → {} at {}",
                    task.input, task.output, shift.building_name,
                ),
            });

            tracing::info!(
                "{} processed {} → {} at {}",
                name.0, task.input, task.output, shift.building_name,
            );

            commands.entity(entity).remove::<ProcessingTask>();
        }
    }

    // Start new processing tasks for idle workers.
    for (entity, name, shift) in &workers_without_task {
        let Ok(mut inv) = building_invs.get_mut(shift.building) else { continue };

        // Find first raw material in the building's inventory.
        let raw = inv.items.iter()
            .find(|(item, count)| item.is_raw_material() && **count > 0)
            .map(|(item, _)| *item);

        let Some(raw_item) = raw else { continue };
        let Some((output, ticks)) = raw_item.processing_recipe() else { continue };

        // Consume one unit of raw material.
        inv.remove(raw_item, 1);

        commands.entity(entity).insert(ProcessingTask {
            input: raw_item,
            output,
            ticks_remaining: ticks,
        });

        tracing::info!(
            "{} started processing {} → {} ({} ticks) at {}",
            name.0, raw_item, output, ticks, shift.building_name,
        );
    }
}

/// System: detect when a customer arrives at a staffable building with no worker.
pub fn shift_demand_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut staffable: Query<(Entity, &crate::world::structures::SpriteType, &mut Staffable)>,
    agents_at_entrance: Query<(&AgentName, &crate::world::map::GridPos, &AgentGoal)>,
    building_entrances: Query<&crate::world::structures::Entrance>,
) {
    // Only check every 10 ticks.
    if tick.0 % 10 != 0 { return; }

    for (building_entity, sprite, mut staffable) in &mut staffable {
        if staffable.worker.is_some() {
            staffable.needs_worker = false;
            continue;
        }

        // Check if any agent is trying to use this building's service.
        let Ok(entrance) = building_entrances.get(building_entity) else { continue };

        let customer_waiting = agents_at_entrance.iter().any(|(_, pos, goal)| {
            pos.x == entrance.0.x && pos.y == entrance.0.y
                && matches!(goal, AgentGoal::GoingToService { .. })
        });

        if customer_waiting && !staffable.needs_worker {
            staffable.needs_worker = true;
            event_log.push(LogEvent {
                tick: tick.0,
                agent: "SYSTEM".into(),
                kind: LogKind::System,
                text: format!("{} needs a worker! Customer waiting.", sprite.0),
            });
            tracing::info!("{} needs a worker — customer waiting", sprite.0);
        }
    }
}

/// System: track ticks worked and handle auto-ejection.
pub fn shift_tracking_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut workers: Query<(
        Entity,
        &AgentName,
        &mut ShiftWorker,
        &mut Needs,
        &mut Inventory,
        &mut AgentGoal,
        &mut ThoughtBubble,
        &mut PaycheckWallet,
    )>,
    mut staffable: Query<&mut Staffable>,
) {
    for (entity, name, mut shift, mut needs, mut inv, mut goal, mut thought, mut wallet) in &mut workers {
        // Detect voluntary exit: AI set goal away from WorkingShift (e.g. LeaveShift).
        let voluntary_exit = !matches!(*goal, AgentGoal::WorkingShift { .. });

        if !voluntary_exit {
            shift.ticks_worked += 1;
        }

        // Food perk: working at cafe/market stops hunger from dropping.
        if let Ok(staff) = staffable.get(shift.building) {
            if staff.food_perk && !voluntary_exit {
                needs.hunger = (needs.hunger + 0.08).min(100.0); // counteract decay
            }
        }

        // Auto-eject on critical needs.
        let should_eject = voluntary_exit
            || needs.energy < 10.0
            || (needs.hunger < 10.0 && !staffable.get(shift.building).is_ok_and(|s| s.food_perk));

        if should_eject {
            let paycheck = Paycheck {
                building_name: shift.building_name.clone(),
                ticks_worked: shift.ticks_worked,
                ticks_per_gold: shift.ticks_per_gold,
            };
            let gold_value = paycheck.gold_value();

            let reason = if voluntary_exit { "Left" } else { "Ejected from" };

            if gold_value > 0 {
                wallet.paychecks.push(paycheck);
                inv.add(ItemType::Paycheck, 1);
                thought.0 = format!(
                    "Shift over at {} — got paycheck worth {}g (redeem at bounty board)",
                    shift.building_name, gold_value,
                );
            } else {
                thought.0 = format!(
                    "Shift over at {} — too short for a paycheck",
                    shift.building_name,
                );
            }

            event_log.push(LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: LogKind::Action,
                text: format!(
                    "{} shift at {} ({}t worked → paycheck {}g)",
                    reason, shift.building_name, shift.ticks_worked, gold_value,
                ),
            });

            tracing::info!(
                "{} {} {} shift ({}t → paycheck {}g)",
                name.0, reason.to_lowercase(), shift.building_name, shift.ticks_worked, gold_value,
            );

            // Clear the staffable slot.
            if let Ok(mut staff) = staffable.get_mut(shift.building) {
                staff.worker = None;
            }

            commands.entity(entity).remove::<ShiftWorker>();
            commands.entity(entity).remove::<ProcessingTask>();
            commands.entity(entity).remove::<InsideBuilding>();
            *goal = AgentGoal::Idle;
        }
    }
}

/// System: redeem paychecks when an agent finishes the `redeem_paycheck` action.
/// Runs after action_timer_system — detects agents who just had their timer removed
/// and whose goal just returned to Idle from a redeem_paycheck action.
pub fn paycheck_redemption_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut agents: Query<(
        &AgentName,
        &mut Inventory,
        &mut PaycheckWallet,
        &mut ThoughtBubble,
        &crate::agents::actions::ActionTimer,
    )>,
) {
    for (name, mut inv, mut wallet, mut thought, timer) in &mut agents {
        if timer.action_name != "redeem_paycheck" || timer.remaining_ticks != 1 {
            continue;
        }
        if wallet.paychecks.is_empty() {
            thought.0 = "No paychecks to redeem.".into();
            continue;
        }

        let count = wallet.paychecks.len();
        let paycheck_items = inv.count(ItemType::Paycheck);
        let gold = wallet.redeem_all();
        if gold > 0 {
            inv.add(ItemType::GoldCoin, gold);
        }
        // Remove the Paycheck item tokens from inventory.
        if paycheck_items > 0 {
            inv.remove(ItemType::Paycheck, paycheck_items);
        }

        thought.0 = format!("Cashed {} paycheck(s) for {}g!", count, gold);

        event_log.push(LogEvent {
            tick: tick.0,
            agent: name.0.clone(),
            kind: LogKind::Action,
            text: format!("Redeemed {} paycheck(s) → {}g", count, gold),
        });

        tracing::info!("{} redeemed {} paycheck(s) for {}g", name.0, count, gold);
    }
}
