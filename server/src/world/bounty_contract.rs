use bevy::prelude::*;
use uuid::Uuid;

use crate::agents::action_log::{ActionEvent, ActionLog};
use crate::tick::TickCount;

/// A bounty contract — a physical item agents carry.
#[derive(Component, Debug, Clone)]
pub struct BountyContract {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub reward_gold: u32,
    /// Steps that must be completed for the bounty to be valid.
    pub steps: Vec<BountyStep>,
    /// Tick when the bounty was created.
    pub created_tick: u64,
    /// Tick when the bounty was picked up by an agent.
    pub picked_up_tick: Option<u64>,
    /// Total ticks allowed from pickup to expiration.
    pub ttl_ticks: u32,
    /// Who currently holds this bounty (None = on the board).
    pub holder: Option<Entity>,
    /// Is this bounty expired?
    pub expired: bool,
}

impl BountyContract {
    pub fn ticks_remaining(&self, current_tick: u64) -> Option<u32> {
        let pickup = self.picked_up_tick?;
        let deadline = pickup + self.ttl_ticks as u64;
        if current_tick >= deadline {
            None // expired
        } else {
            Some((deadline - current_tick) as u32)
        }
    }

    pub fn is_expired(&self, current_tick: u64) -> bool {
        self.picked_up_tick
            .is_some_and(|pickup| current_tick >= pickup + self.ttl_ticks as u64)
    }

    /// Check all steps against an agent's action log.
    pub fn verify_steps(&self, log: &ActionLog) -> Vec<bool> {
        let start = self.picked_up_tick.unwrap_or(0);
        let end = start + self.ttl_ticks as u64;

        self.steps
            .iter()
            .map(|step| step.verify(log, start, end, self.id))
            .collect()
    }

    pub fn all_steps_complete(&self, log: &ActionLog) -> bool {
        self.verify_steps(log).iter().all(|v| *v)
    }
}

/// A single verifiable step in a bounty contract.
#[derive(Debug, Clone)]
pub struct BountyStep {
    pub description: String,
    pub condition: StepCondition,
}

#[derive(Debug, Clone)]
pub enum StepCondition {
    /// Agent must spend at least N gold at a specific building.
    SpendGold { building: String, amount: u32 },
    /// Agent must use a specific service.
    UseService { service: String },
    /// Agent must perform a web search at Google.
    WebSearch { min_count: usize },
    /// Agent must produce a document with the given title.
    ProduceDocument { title: String },
    /// Agent must visit (enter) a specific building.
    VisitBuilding { building: String },
    /// Agent must return all items to the bounty board.
    ReturnToBoard,
}

impl BountyStep {
    fn verify(&self, log: &ActionLog, start: u64, end: u64, bounty_id: Uuid) -> bool {
        match &self.condition {
            StepCondition::SpendGold { building, amount } => {
                let total: u32 = log
                    .entries
                    .iter()
                    .filter(|e| e.tick >= start && e.tick <= end)
                    .filter_map(|e| match &e.event {
                        ActionEvent::GoldSpent { amount: a, building: b } if b == building => Some(*a),
                        _ => None,
                    })
                    .sum();
                total >= *amount
            }

            StepCondition::UseService { service } => {
                log.has_between(start, end, |ev| {
                    matches!(ev, ActionEvent::ServiceUsed { service: s, .. } if s == service)
                })
            }

            StepCondition::WebSearch { min_count } => {
                log.count_between(start, end, |ev| {
                    matches!(ev, ActionEvent::WebSearched { .. })
                }) >= *min_count
            }

            StepCondition::ProduceDocument { title } => {
                log.has_between(start, end, |ev| {
                    matches!(ev, ActionEvent::DocumentProduced { title: t, bounty_id: bid } if t == title && *bid == bounty_id)
                })
            }

            StepCondition::VisitBuilding { building } => {
                log.has_between(start, end, |ev| {
                    matches!(ev, ActionEvent::EnteredBuilding { building: b } if b == building)
                })
            }

            StepCondition::ReturnToBoard => {
                log.has_between(start, end, |ev| {
                    matches!(ev, ActionEvent::BountyReturned { bounty_id: bid } if *bid == bounty_id)
                })
            }
        }
    }
}

/// The bounty board holds available contracts.
#[derive(Resource, Default)]
pub struct ContractBoard {
    pub contracts: Vec<BountyContract>,
}

impl ContractBoard {
    pub fn available(&self) -> Vec<&BountyContract> {
        self.contracts.iter().filter(|c| c.holder.is_none() && !c.expired).collect()
    }
}

/// System: check for expired bounties in agent inventories.
pub fn bounty_expiry_system(
    tick: Res<TickCount>,
    mut contracts: ResMut<ContractBoard>,
) {
    for contract in &mut contracts.contracts {
        if contract.holder.is_some() && !contract.expired && contract.is_expired(tick.0) {
            contract.expired = true;
            tracing::info!("Bounty '{}' has EXPIRED", contract.title);
        }
    }
}

/// Recycle cost for expired bounties.
pub const RECYCLE_COST: u32 = 1;
