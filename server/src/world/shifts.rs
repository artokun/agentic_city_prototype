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
#[allow(dead_code)]
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

pub fn redeem_paychecks(inv: &mut Inventory, wallet: &mut PaycheckWallet) -> Option<(usize, u32)> {
    if wallet.paychecks.is_empty() {
        return None;
    }

    let count = wallet.paychecks.len();
    let paycheck_items = inv.count(ItemType::Paycheck);
    let gold = wallet.redeem_all();

    if gold > 0 {
        inv.add(ItemType::GoldCoin, gold);
    }
    if paycheck_items > 0 {
        inv.remove(ItemType::Paycheck, paycheck_items);
    }

    Some((count, gold))
}

/// System: shift workers process raw materials into finished goods.
pub fn shift_processing_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    workers_without_task: Query<(Entity, &AgentName, &ShiftWorker), Without<ProcessingTask>>,
    mut workers_with_task: Query<(Entity, &AgentName, &ShiftWorker, &mut ProcessingTask)>,
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
                name.0,
                task.input,
                task.output,
                shift.building_name,
            );

            commands.entity(entity).remove::<ProcessingTask>();
        }
    }

    // Start new processing tasks for idle workers.
    // Only process at retail locations (cafe, market) — NOT the warehouse.
    for (entity, name, shift) in &workers_without_task {
        if shift.building_name == "warehouse" || shift.building_name == "hotel" {
            continue; // Warehouse is wholesale only, hotel doesn't process food.
        }
        let Ok(mut inv) = building_invs.get_mut(shift.building) else {
            continue;
        };

        // Find first raw material in the building's inventory.
        let raw = inv
            .items
            .iter()
            .find(|(item, count)| item.is_raw_material() && **count > 0)
            .map(|(item, _)| *item);

        let Some(raw_item) = raw else { continue };
        let Some((output, ticks)) = raw_item.processing_recipe() else {
            continue;
        };

        // Consume one unit of raw material.
        inv.remove(raw_item, 1);

        commands.entity(entity).insert(ProcessingTask {
            input: raw_item,
            output,
            ticks_remaining: ticks,
        });

        tracing::info!(
            "{} started processing {} → {} ({} ticks) at {}",
            name.0,
            raw_item,
            output,
            ticks,
            shift.building_name,
        );
    }
}

/// System: detect when a customer arrives at a staffable building with no worker.
pub fn shift_demand_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut staffable: Query<(
        Entity,
        &crate::world::structures::SpriteType,
        &mut Staffable,
    )>,
    agents_at_entrance: Query<(&AgentName, &crate::world::map::GridPos, &AgentGoal)>,
    building_entrances: Query<&crate::world::structures::Entrance>,
) {
    // Only check every 10 ticks.
    if tick.0 % 10 != 0 {
        return;
    }

    for (building_entity, sprite, mut staffable) in &mut staffable {
        if staffable.worker.is_some() {
            staffable.needs_worker = false;
            continue;
        }

        // Check if any agent is trying to use this building's service.
        let Ok(entrance) = building_entrances.get(building_entity) else {
            continue;
        };

        let customer_waiting = agents_at_entrance.iter().any(|(_, pos, goal)| {
            pos.x == entrance.0.x
                && pos.y == entrance.0.y
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
        Option<&crate::agents::components::WantsToLeaveShift>,
    )>,
    mut staffable: Query<&mut Staffable>,
) {
    for (
        entity,
        name,
        mut shift,
        mut needs,
        mut inv,
        mut goal,
        mut thought,
        mut wallet,
        wants_leave,
    ) in
        &mut workers
    {
        // Keep an active shift stable unless the agent explicitly leaves or critical needs eject them.
        let voluntary_exit = wants_leave.is_some();

        if !voluntary_exit {
            *goal = AgentGoal::WorkingShift {
                building: shift.building,
            };
        }

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

            let reason = if voluntary_exit {
                "Left"
            } else {
                "Ejected from"
            };

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
                name.0,
                reason.to_lowercase(),
                shift.building_name,
                shift.ticks_worked,
                gold_value,
            );

            // Clear the staffable slot.
            if let Ok(mut staff) = staffable.get_mut(shift.building) {
                staff.worker = None;
            }

            commands.entity(entity).remove::<ShiftWorker>();
            commands.entity(entity).remove::<ProcessingTask>();
            commands.entity(entity).remove::<InsideBuilding>();
            commands
                .entity(entity)
                .remove::<crate::agents::components::WantsToLeaveShift>();
            *goal = AgentGoal::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paycheck(ticks_worked: u32, ticks_per_gold: u32) -> Paycheck {
        Paycheck {
            building_name: "test".into(),
            ticks_worked,
            ticks_per_gold,
        }
    }

    // ---- Paycheck gold_value ----

    #[test]
    fn gold_value_integer_division() {
        let p = paycheck(25, 10);
        assert_eq!(p.gold_value(), 2); // 25 / 10 = 2 (truncated)
    }

    #[test]
    fn gold_value_exact() {
        let p = paycheck(30, 10);
        assert_eq!(p.gold_value(), 3);
    }

    #[test]
    fn gold_value_below_threshold_yields_zero() {
        let p = paycheck(5, 10);
        assert_eq!(p.gold_value(), 0);
    }

    #[test]
    fn gold_value_zero_ticks() {
        let p = paycheck(0, 10);
        assert_eq!(p.gold_value(), 0);
    }

    // ---- PaycheckWallet ----

    #[test]
    fn wallet_total_gold() {
        let mut w = PaycheckWallet::default();
        w.paychecks.push(paycheck(30, 10)); // 3g
        w.paychecks.push(paycheck(20, 10)); // 2g
        assert_eq!(w.total_gold(), 5);
    }

    #[test]
    fn redeem_all_clears_and_returns_total() {
        let mut w = PaycheckWallet::default();
        w.paychecks.push(paycheck(30, 10)); // 3g
        w.paychecks.push(paycheck(50, 10)); // 5g
        let gold = w.redeem_all();
        assert_eq!(gold, 8);
        assert!(w.paychecks.is_empty());
        assert_eq!(w.total_gold(), 0);
    }

    #[test]
    fn redeem_all_empty_wallet() {
        let mut w = PaycheckWallet::default();
        assert_eq!(w.redeem_all(), 0);
        assert!(w.paychecks.is_empty());
    }

    #[test]
    fn redeem_paychecks_moves_value_into_gold() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Paycheck, 2);

        let mut w = PaycheckWallet::default();
        w.paychecks.push(paycheck(30, 10)); // 3g
        w.paychecks.push(paycheck(50, 10)); // 5g

        assert_eq!(redeem_paychecks(&mut inv, &mut w), Some((2, 8)));
        assert_eq!(inv.count(ItemType::Paycheck), 0);
        assert_eq!(inv.count(ItemType::GoldCoin), 8);
        assert!(w.paychecks.is_empty());
    }

    #[test]
    fn redeem_paychecks_empty_wallet_is_noop() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Paycheck, 1);
        let mut w = PaycheckWallet::default();

        assert_eq!(redeem_paychecks(&mut inv, &mut w), None);
        assert_eq!(inv.count(ItemType::Paycheck), 1);
        assert_eq!(inv.count(ItemType::GoldCoin), 0);
    }
}
