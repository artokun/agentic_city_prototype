//! Hospital system: agents pass out when energy/hunger hits 0.
//! A rescue bounty is posted. Another agent carries them to the hospital.
//! Treatment restores stats using rations. Costs 5g (can go into debt)
//! + 5g reward to the rescuer.

use bevy::prelude::*;
use uuid::Uuid;

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::token_tracking::ContextWindow;
use crate::config;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyBoard, BountyObjective, BountyState, BountyTokenStore};
use crate::world::map::GridPos;
use crate::world::map::TileInventory;

// Hospital constants now in config.rs.

/// Marks an agent as incapacitated — can't think, move, or act.
#[derive(Component)]
pub struct Incapacitated {
    pub reason: String,
    pub passed_out_tick: u64,
    pub rescue_bounty_id: Option<Uuid>,
}

/// Marks an agent as recovering in the hospital.
#[derive(Component)]
pub struct Recovering {
    pub ticks_remaining: u32,
}

/// System: detect agents whose energy or hunger hits 0 → incapacitate them.
pub fn pass_out_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut boards_hospital: Query<&mut BountyTokenStore, With<BountyBoard>>,
    mut tile_inventory: ResMut<TileInventory>,
    mut agents: Query<
        (
            Entity,
            &AgentName,
            &GridPos,
            &mut Needs,
            &mut Inventory,
            &mut AgentGoal,
            &mut ThoughtBubble,
        ),
        Without<Incapacitated>,
    >,
) {
    let Some(mut bounty_registry) = boards_hospital.iter_mut().next() else {
        return;
    };
    for (entity, name, pos, mut needs, mut inv, mut goal, mut thought) in &mut agents {
        let passed_out = needs.energy <= 0.0 || needs.hunger <= 0.0;
        if !passed_out {
            continue;
        }

        let reason = if needs.energy <= 0.0 {
            "exhaustion"
        } else {
            "starvation"
        };

        tracing::info!(
            "{} PASSED OUT from {} at ({},{})",
            name.0,
            reason,
            pos.x,
            pos.y
        );

        // Deduct hospital fee (can go into debt).
        inv.deduct_gold_with_debt(config::hospital_fee());

        // Post rescue bounty with clear instructions.
        let bounty_id = Uuid::new_v4();
        let mut bounty = Bounty::simple(
            bounty_id,
            format!(
                "RESCUE: Carry {} to the hospital (passed out at ({},{}))",
                name.0, pos.x, pos.y
            ),
            BountyObjective::WorkAtBuilding,
            config::rescue_reward(),
            vec![],
        );
        bounty.hidden_criteria = format!(
            "Instructions for agent: Step 1: Go to ({},{}) where {} passed out. \
             Step 2: Use take_item with service='body:{}' to pick up their body. \
             Step 3: Go to the hospital using go_to_service with building='hospital'. \
             Step 4: Use deposit_item with service='body:{}' to drop them off at the hospital. \
             Step 5: Return to the bounty board, deposit your bounty_token, and complete_bounty.\n\n\
             GM: Verify the rescuer picked up body:{} (take_item in action log) AND deposited body:{} at the hospital (deposit_item in action log). \
             If they just walked to the hospital without carrying the body, REJECT.",
            pos.x, pos.y, name.0, name.0, name.0, name.0, name.0
        );
        bounty_registry.tokens.insert(bounty.id, bounty);

        // Freeze the agent — they lay where they fell as a tile item.
        // Another agent must pick them up and carry them to the hospital.
        commands.entity(entity).insert(Incapacitated {
            reason: reason.into(),
            passed_out_tick: tick.0,
            rescue_bounty_id: Some(bounty_id),
        });

        // Drop the agent as a tile item — can be picked up by rescuers.
        tile_inventory.drop_item(pos.x, pos.y, format!("body:{}", name.0));
        tracing::info!(
            "{} is now a tile item at ({},{}) — needs rescue!",
            name.0,
            pos.x,
            pos.y
        );

        // Clamp needs to 0.
        needs.energy = 0.0;
        needs.hunger = 0.0;

        // Stop all goals.
        *goal = AgentGoal::Idle;
        thought.0 = format!("*passed out from {}*", reason);

        event_log.push(LogEvent {
            tick: tick.0,
            agent: name.0.clone(),
            kind: LogKind::System,
            text: format!(
                "PASSED OUT from {}! Hospital fee: {}g. Rescue bounty posted.",
                reason,
                config::hospital_fee()
            ),
        });
    }
}

/// System: recover agents in the hospital.
pub fn hospital_recovery_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    sessions: Res<AgentSessions>,
    mut patients: Query<(
        Entity,
        &AgentName,
        &mut Recovering,
        &mut Needs,
        &mut ThoughtBubble,
        &mut ContextWindow,
    )>,
) {
    for (entity, name, mut recovery, mut needs, mut thought, mut ctx_window) in &mut patients {
        recovery.ticks_remaining = recovery.ticks_remaining.saturating_sub(1);

        // Don't restore energy directly — it's token-driven.
        // Energy will be restored when compaction resets tokens_used.
        needs.hunger = (needs.hunger + 0.2).min(60.0);

        if recovery.ticks_remaining == 0 {
            // Send compaction command via the session abstraction.
            sessions.send_compact(&entity);
            ctx_window.tokens_used = 0;
            tracing::info!(
                "[HOSPITAL COMPACT] {} — compact sent, tokens reset",
                name.0
            );

            tracing::info!("{} has recovered in the hospital!", name.0);
            thought.0 = "Waking up in the hospital... feeling groggy but rested.".into();

            commands.entity(entity).remove::<Recovering>();
            commands.entity(entity).remove::<Incapacitated>();

            event_log.push(LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: LogKind::System,
                text: "Recovered in the hospital. Back on your feet!".into(),
            });
        }
    }
}
