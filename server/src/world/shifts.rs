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
    )>,
    mut staffable: Query<&mut Staffable>,
) {
    for (entity, name, mut shift, mut needs, mut inv, mut goal, mut thought) in &mut workers {
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
            let gold_earned = shift.ticks_worked / shift.ticks_per_gold;
            if gold_earned > 0 {
                inv.add(ItemType::GoldCoin, gold_earned);
            }

            let reason = if voluntary_exit { "Left" } else { "Ejected from" };

            thought.0 = format!(
                "Shift over at {} — earned {}g for {} ticks",
                shift.building_name, gold_earned, shift.ticks_worked,
            );

            event_log.push(LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: LogKind::Action,
                text: format!(
                    "{} shift at {} ({}t worked → {}g)",
                    reason, shift.building_name, shift.ticks_worked, gold_earned,
                ),
            });

            tracing::info!(
                "{} {} {} shift ({}t → {}g)",
                name.0, reason.to_lowercase(), shift.building_name, shift.ticks_worked, gold_earned,
            );

            // Clear the staffable slot.
            if let Ok(mut staff) = staffable.get_mut(shift.building) {
                staff.worker = None;
            }

            commands.entity(entity).remove::<ShiftWorker>();
            commands.entity(entity).remove::<InsideBuilding>();
            *goal = AgentGoal::Idle;
        }
    }
}
