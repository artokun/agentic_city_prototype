use bevy::prelude::*;

use super::action_log::{ActionEvent, ActionLog};
use super::ai::AgentSessions;
use super::components::{AgentAnimation, AgentId, AnimState, ThoughtBubble};
use super::needs::Needs;
use super::token_tracking::ContextWindow;
use crate::items::{DocumentInventory, Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::economy::GoldReserve;
use crate::world::services::ServiceEffects;
use crate::world::structures::{InsideBuilding, SpriteType, StructureId};

/// While this component is present, the agent is busy and cannot act.
#[derive(Component)]
pub struct ActionTimer {
    pub action_name: String,
    pub remaining_ticks: u32,
    pub effects: ServiceEffects,
    pub gold_cost: u32,
    pub paid: bool,
    /// Item consumed from the building's inventory when the agent pays.
    pub consumes_item: Option<ItemType>,
    /// Item produced and given to the agent when the action finishes.
    pub produces_item: Option<ItemType>,
}

/// System: tick action timers. When done, apply effects and remove.
pub fn action_timer_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut agents: Query<
        (
            Entity,
            &mut ActionTimer,
            &mut Needs,
            &mut Inventory,
            &mut AgentAnimation,
            &mut ThoughtBubble,
            &super::components::AgentName,
            &AgentId,
            &mut ActionLog,
            &mut ContextWindow,
            Option<&InsideBuilding>,
        ),
        Without<StructureId>,
    >,
    sessions: Res<AgentSessions>,
    mut building_reserves: Query<&mut GoldReserve>,
    mut building_inventories: Query<&mut Inventory, With<StructureId>>,
    building_names: Query<&SpriteType, With<StructureId>>,
) {
    for (entity, mut timer, mut needs, mut inv, mut anim, mut thought, name, agent_id, mut action_log, mut ctx_window, inside) in &mut agents {
        // Resolve the building name for event logging.
        let building_name = inside
            .and_then(|ib| building_names.get(ib.0).ok())
            .map(|s| s.0.clone())
            .unwrap_or_default();

        // Pay gold on first tick — credit the building's reserve.
        if !timer.paid && timer.gold_cost > 0 {
            if inv.has(ItemType::GoldCoin, timer.gold_cost) {
                inv.remove(ItemType::GoldCoin, timer.gold_cost);

                // Credit building revenue.
                if let Some(inside) = inside {
                    if let Ok(mut reserve) = building_reserves.get_mut(inside.0) {
                        reserve.0 += timer.gold_cost as i32;
                    }

                    // Consume item from building inventory (e.g. coffee from cafe).
                    if let Some(item) = timer.consumes_item {
                        if let Ok(mut bld_inv) = building_inventories.get_mut(inside.0) {
                            bld_inv.remove(item, 1);
                        }
                    }
                }

                action_log.log(tick.0, ActionEvent::GoldSpent {
                    amount: timer.gold_cost,
                    building: building_name.clone(),
                });

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

            // Give produced item if any.
            if let Some(item) = timer.produces_item {
                inv.add(item, 1);
                tracing::info!("{} received {} from '{}'", name.0, item, timer.action_name);
            }

            tracing::info!(
                "{} finished '{}' (E:{:.0} H:{:.0} B:{:.0})",
                name.0,
                timer.action_name,
                needs.energy,
                needs.hunger,
                needs.boredom,
            );

            action_log.log(tick.0, ActionEvent::ServiceUsed {
                service: timer.action_name.clone(),
                building: building_name.clone(),
            });

            // Sleep actions trigger context compaction.
            let is_sleep = timer.action_name.contains("sleep");
            if is_sleep {
                // Send /compact to trigger real context compaction.
                if let Some(session) = sessions.sessions.get(&entity) {
                    let _ = session.prompt_tx.try_send("/compact".to_string());
                }
                // Reset our token counter — real compaction reduces context.
                let old = ctx_window.tokens_used;
                ctx_window.tokens_used = 0;
                tracing::info!(
                    "[COMPACT] {} slept → /compact sent, tokens {} → 0",
                    name.0, old,
                );
                thought.0 = "Woke up refreshed! Energy restored.".into();
            } else {
                thought.0 = format!("Finished {}.", timer.action_name);
            }

            anim.0 = AnimState::Idle;
            commands.entity(entity).remove::<ActionTimer>();
        }
    }
}
