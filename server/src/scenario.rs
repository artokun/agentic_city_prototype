//! Scenario test framework for running the real Bevy ECS game with injectable configs.
//! Used by integration tests to spawn isolated game instances with custom agents, bounties,
//! and deterministic shutdown.

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use std::sync::{Arc, Mutex};

use crate::agents::components::*;
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyBoard, BountyObjective, BountyTokenStore};
use crate::world::map::GridPos;

// ---------------------------------------------------------------------------
// Scenario agent config
// ---------------------------------------------------------------------------

/// Resource: overrides the default Alice/Bob/Carol agent list.
#[derive(Resource, Clone)]
pub struct ScenarioAgentConfig {
    pub agents: Vec<ScenarioAgent>,
}

#[derive(Clone)]
pub struct ScenarioAgent {
    pub name: String,
    pub model: String,
    pub speed: f32,
}

// ---------------------------------------------------------------------------
// Scenario bounty config
// ---------------------------------------------------------------------------

/// Resource: seeds specific bounties instead of the default templates.
#[derive(Resource, Clone)]
pub struct ScenarioBountyConfig {
    pub bounties: Vec<ScenarioBounty>,
}

#[derive(Clone)]
pub struct ScenarioBounty {
    pub title: String,
    pub reward: u32,
    pub hidden_criteria: String,
    pub objective: BountyObjective,
    pub claim_items: Vec<(ItemType, u32)>,
}

// ---------------------------------------------------------------------------
// World snapshot
// ---------------------------------------------------------------------------

/// Periodically captured world state for test assertions.
#[derive(Debug, Clone, Default)]
pub struct WorldSnapshot {
    pub tick: u64,
    pub agents: Vec<AgentSnapshot>,
    pub bounties: Vec<BountySnapshot>,
}

#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub name: String,
    pub gold: u32,
    pub gold_debt: u32,
    pub position: (i32, i32),
    pub goal: String,
    pub energy: f32,
    pub hunger: f32,
    pub boredom: f32,
    pub inventory_items: Vec<(String, u32)>,
}

#[derive(Debug, Clone)]
pub struct BountySnapshot {
    pub description: String,
    pub reward: u32,
    pub state: String,
    pub claimed_by: Option<String>,
}

// ---------------------------------------------------------------------------
// Observer resource
// ---------------------------------------------------------------------------

/// Bevy resource: shared snapshot that the test harness reads from another thread.
#[derive(Resource, Clone)]
pub struct ScenarioObserver(pub Arc<Mutex<WorldSnapshot>>);

// ---------------------------------------------------------------------------
// Max ticks shutdown
// ---------------------------------------------------------------------------

/// Resource: automatically exits the Bevy app after this many ticks.
#[derive(Resource)]
pub struct MaxTicks(pub u64);

// ---------------------------------------------------------------------------
// Shutdown flag
// ---------------------------------------------------------------------------

/// Resource: test harness can set this to request graceful shutdown.
#[derive(Resource)]
pub struct ShutdownFlag(pub Arc<Mutex<bool>>);

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

/// System: capture world state every tick into the shared observer.
pub fn snapshot_system(
    tick: Res<TickCount>,
    observer: Res<ScenarioObserver>,
    agents: Query<(&AgentName, &Inventory, &GridPos, &AgentGoal, &Needs)>,
    boards: Query<&BountyTokenStore, With<BountyBoard>>,
    agent_names: Query<&AgentName>,
) {
    let mut snap = WorldSnapshot {
        tick: tick.0,
        agents: Vec::new(),
        bounties: Vec::new(),
    };

    for (name, inv, pos, goal, needs) in &agents {
        let inventory_items: Vec<(String, u32)> = inv
            .items
            .iter()
            .map(|(k, v)| (format!("{}", k), *v))
            .collect();

        snap.agents.push(AgentSnapshot {
            name: name.0.clone(),
            gold: inv.count(ItemType::GoldCoin),
            gold_debt: inv.gold_debt,
            position: (pos.x, pos.y),
            goal: format!("{:?}", goal),
            energy: needs.energy,
            hunger: needs.hunger,
            boredom: needs.boredom,
            inventory_items,
        });
    }

    if let Some(store) = boards.iter().next() {
        for bounty in store.tokens.values() {
            let claimed_by_name = bounty
                .claimed_by
                .and_then(|e| agent_names.get(e).ok().map(|n| n.0.clone()));

            snap.bounties.push(BountySnapshot {
                description: bounty.description.clone(),
                reward: bounty.reward_gold,
                state: format!("{:?}", bounty.state),
                claimed_by: claimed_by_name,
            });
        }
    }

    if let Ok(mut guard) = observer.0.lock() {
        *guard = snap;
    }
}

/// System: exit the app when tick exceeds MaxTicks.
pub fn shutdown_system(
    tick: Res<TickCount>,
    max_ticks: Res<MaxTicks>,
    mut exit: MessageWriter<AppExit>,
) {
    if tick.0 >= max_ticks.0 {
        tracing::info!(
            "[SCENARIO] Reached max ticks ({}), shutting down",
            max_ticks.0
        );
        exit.write(AppExit::Success);
    }
}

/// System: exit when the shutdown flag is set by the test harness.
pub fn shutdown_flag_system(flag: Res<ShutdownFlag>, mut exit: MessageWriter<AppExit>) {
    if let Ok(guard) = flag.0.lock() {
        if *guard {
            tracing::info!("[SCENARIO] Shutdown flag set, exiting");
            exit.write(AppExit::Success);
        }
    }
}

/// System: seed bounties from ScenarioBountyConfig (runs once on startup tick).
pub fn seed_scenario_bounties_system(
    tick: Res<TickCount>,
    config: Option<Res<ScenarioBountyConfig>>,
    mut boards: Query<&mut BountyTokenStore, With<BountyBoard>>,
    mut injector: ResMut<crate::world::bounty_injector::InjectorState>,
) {
    // Only run on tick 0.
    if tick.0 != 0 {
        return;
    }

    let Some(config) = config else { return };

    // Mark the normal injector as already seeded so it doesn't run.
    injector.seeded = true;

    let Some(mut store) = boards.iter_mut().next() else {
        return;
    };

    for sb in &config.bounties {
        let mut bounty = Bounty::simple(
            uuid::Uuid::new_v4(),
            sb.title.clone(),
            sb.objective.clone(),
            sb.reward,
            sb.claim_items.clone(),
        );
        bounty.hidden_criteria = sb.hidden_criteria.clone();
        store.tokens.insert(bounty.id, bounty);
    }

    tracing::info!(
        "[SCENARIO] Seeded {} scenario bounties",
        config.bounties.len()
    );
}
