use bevy::prelude::*;

use super::components::{AgentAnimation, AnimState, ThoughtBubble};
use super::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::world::services::ServiceEffects;

/// While this component is present, the agent is busy and cannot act.
#[derive(Component)]
pub struct ActionTimer {
    pub action_name: String,
    pub remaining_ticks: u32,
    pub effects: ServiceEffects,
    pub gold_cost: u32,
    pub paid: bool,
}

/// System: tick action timers. When done, apply effects and remove.
pub fn action_timer_system(
    mut commands: Commands,
    mut agents: Query<(
        Entity,
        &mut ActionTimer,
        &mut Needs,
        &mut Inventory,
        &mut AgentAnimation,
        &mut ThoughtBubble,
        &super::components::AgentName,
    )>,
) {
    for (entity, mut timer, mut needs, mut inv, mut anim, mut thought, name) in &mut agents {
        // Pay gold on first tick.
        if !timer.paid && timer.gold_cost > 0 {
            if inv.has(ItemType::GoldCoin, timer.gold_cost) {
                inv.remove(ItemType::GoldCoin, timer.gold_cost);
                timer.paid = true;
            } else {
                // Can't afford — cancel action.
                thought.0 = "Can't afford this...".into();
                commands.entity(entity).remove::<ActionTimer>();
                anim.0 = AnimState::Idle;
                continue;
            }
        } else if !timer.paid {
            timer.paid = true;
        }

        timer.remaining_ticks = timer.remaining_ticks.saturating_sub(1);
        anim.0 = AnimState::Working;

        if timer.remaining_ticks == 0 {
            // Apply effects.
            needs.energy += timer.effects.energy;
            needs.hunger += timer.effects.hunger;
            needs.boredom += timer.effects.boredom;
            needs.clamp();

            tracing::info!(
                "{} finished '{}' (E:{:.0} H:{:.0} B:{:.0})",
                name.0,
                timer.action_name,
                needs.energy,
                needs.hunger,
                needs.boredom,
            );

            thought.0 = format!("Finished {}.", timer.action_name);
            anim.0 = AnimState::Idle;
            commands.entity(entity).remove::<ActionTimer>();
        }
    }
}
