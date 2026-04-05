//! Hospital system: agents pass out when energy/hunger hits 0.
//! A rescue bounty is posted. Another agent carries them to the hospital.
//! Treatment restores stats using rations. Costs 5g (can go into debt)
//! + 5g reward to the rescuer.

use bevy::prelude::*;
use uuid::Uuid;

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::token_tracking::ContextWindow;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};
use crate::world::map::GridPos;

/// Hospital treatment cost.
pub const HOSPITAL_FEE: u32 = 5;
/// Reward for the agent who carries someone to the hospital.
pub const RESCUE_REWARD: u32 = 5;
/// Ticks to recover in the hospital.
pub const RECOVERY_TICKS: u32 = 300; // ~30 seconds

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
    mut bounty_registry: ResMut<BountyRegistry>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos, &mut Needs, &mut Inventory,
        &mut AgentGoal, &mut ThoughtBubble,
    ), Without<Incapacitated>>,
) {
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

        tracing::info!("{} PASSED OUT from {} at ({},{})", name.0, reason, pos.x, pos.y);

        // Deduct hospital fee (can go into debt).
        inv.deduct_gold_with_debt(HOSPITAL_FEE);

        // Post rescue bounty.
        let bounty_id = Uuid::new_v4();
        let bounty = Bounty::simple(
            bounty_id,
            format!("RESCUE: Carry {} to the hospital (passed out at ({},{}))", name.0, pos.x, pos.y),
            BountyObjective::WorkAtBuilding, // rescuer goes to hospital with the agent
            RESCUE_REWARD,
            vec![],
        );
        bounty_registry.bounties.push(bounty);

        // Freeze the agent.
        commands.entity(entity).insert(Incapacitated {
            reason: reason.into(),
            passed_out_tick: tick.0,
            rescue_bounty_id: Some(bounty_id),
        });

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
            text: format!("PASSED OUT from {}! Hospital fee: {}g. Rescue bounty posted.", reason, HOSPITAL_FEE),
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
        Entity, &AgentName, &mut Recovering, &mut Needs, &mut ThoughtBubble,
        &mut ContextWindow,
    )>,
) {
    for (entity, name, mut recovery, mut needs, mut thought, mut ctx_window) in &mut patients {
        recovery.ticks_remaining = recovery.ticks_remaining.saturating_sub(1);

        // Don't restore energy directly — it's token-driven.
        // Energy will be restored when compaction resets tokens_used.
        needs.hunger = (needs.hunger + 0.2).min(60.0);

        if recovery.ticks_remaining == 0 {
            // Compact context on hospital discharge.
            if let Some(session) = sessions.sessions.get(&entity) {
                let _ = session.prompt_tx.try_send("/compact".to_string());
                let old = ctx_window.tokens_used;
                ctx_window.tokens_used = old / 10;
                tracing::info!("[HOSPITAL COMPACT] {} — {} → {} tokens", name.0, old, ctx_window.tokens_used);
            }

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
