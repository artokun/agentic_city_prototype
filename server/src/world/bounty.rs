use bevy::prelude::*;
use std::collections::VecDeque;
use uuid::Uuid;

use crate::agents::action_log::{ActionEvent, ActionLog};
use crate::items::{DocumentInventory, ItemType};
use crate::tick::TickCount;

/// Marker: this structure is a bounty board.
#[derive(Component)]
pub struct BountyBoard;

/// Queue state for a bounty board entity.
#[derive(Component, Default)]
pub struct BoardQueue {
    /// The agent currently interacting with the board (exclusive access).
    pub interacting: Option<Entity>,
    /// Agents waiting for their turn.
    pub waiting: VecDeque<Entity>,
}

impl BoardQueue {
    /// Try to start interacting. Returns true if this agent got access.
    pub fn try_interact(&mut self, agent: Entity) -> bool {
        if self.interacting.is_none() {
            self.interacting = Some(agent);
            true
        } else if self.interacting == Some(agent) {
            true
        } else {
            false
        }
    }

    /// Join the wait queue (if not already in it and not interacting).
    pub fn join_queue(&mut self, agent: Entity) {
        if self.interacting == Some(agent) {
            return;
        }
        if !self.waiting.contains(&agent) {
            self.waiting.push_back(agent);
        }
    }

    /// Leave the board — stop interacting or leave the queue.
    pub fn leave(&mut self, agent: Entity) {
        if self.interacting == Some(agent) {
            self.interacting = None;
        }
        self.waiting.retain(|e| *e != agent);
    }

    /// Advance the queue: if no one is interacting, pop the next waiter.
    pub fn advance(&mut self) -> Option<Entity> {
        if self.interacting.is_none() {
            if let Some(next) = self.waiting.pop_front() {
                self.interacting = Some(next);
                return Some(next);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BountyState {
    Available,
    Claimed,
    /// Awaiting game master verification before payout.
    PendingVerification,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BountyObjective {
    /// Hide the given item inside any structure's inventory.
    HideItem(ItemType),
    /// Find the given item (search structures, pick it up).
    FindItem(ItemType),
    /// Work at the building that posted this job.
    WorkAtBuilding,
    /// Buy items from warehouse and deliver to a building.
    RestockDelivery {
        item: ItemType,
        quantity: u32,
        destination: String,
    },
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
    fn verify(
        &self,
        log: &ActionLog,
        start: u64,
        end: u64,
        bounty_id: Uuid,
        docs: Option<&DocumentInventory>,
    ) -> bool {
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
                docs.is_some_and(|d| d.has(title))
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

/// A bounty — a physical item agents carry. Max 1 active bounty per agent.
#[derive(Debug, Clone)]
pub struct Bounty {
    pub id: Uuid,
    pub description: String,
    pub objective: BountyObjective,
    pub reward_gold: u32,
    pub state: BountyState,
    pub claimed_by: Option<Entity>,
    /// Items given to the agent upon claiming (e.g., the egg to hide).
    pub claim_items: Vec<(ItemType, u32)>,
    /// Tick when the bounty was created.
    pub created_tick: u64,
    /// Tick when the bounty was picked up by an agent.
    pub picked_up_tick: Option<u64>,
    /// Total ticks allowed from pickup to expiration (0 = no TTL).
    pub ttl_ticks: u32,
    /// Is this bounty expired?
    pub expired: bool,
    /// Optional multi-step verification (for contract-style bounties).
    pub steps: Vec<BountyStep>,
    /// Hidden acceptance criteria — only the game master can see this.
    /// Agents never see this field. The GM uses it to verify completion.
    pub hidden_criteria: String,
}

impl Bounty {
    /// Create a simple bounty (no TTL, no steps).
    pub fn simple(
        id: Uuid,
        description: String,
        objective: BountyObjective,
        reward_gold: u32,
        claim_items: Vec<(ItemType, u32)>,
    ) -> Self {
        Self {
            id,
            description,
            objective,
            reward_gold,
            state: BountyState::Available,
            claimed_by: None,
            claim_items,
            created_tick: 0,
            picked_up_tick: None,
            ttl_ticks: 0,
            expired: false,
            steps: vec![],
            hidden_criteria: String::new(),
        }
    }

    /// Create a contract-style bounty with TTL and verification steps.
    pub fn contract(
        id: Uuid,
        description: String,
        objective: BountyObjective,
        reward_gold: u32,
        created_tick: u64,
        ttl_ticks: u32,
        steps: Vec<BountyStep>,
    ) -> Self {
        Self {
            id,
            description,
            objective,
            reward_gold,
            state: BountyState::Available,
            claimed_by: None,
            claim_items: vec![],
            created_tick,
            picked_up_tick: None,
            ttl_ticks,
            expired: false,
            steps,
            hidden_criteria: String::new(),
        }
    }

    pub fn ticks_remaining(&self, current_tick: u64) -> Option<u32> {
        if self.ttl_ticks == 0 {
            return Some(u32::MAX); // no TTL
        }
        let pickup = self.picked_up_tick?;
        let deadline = pickup + self.ttl_ticks as u64;
        if current_tick >= deadline {
            None // expired
        } else {
            Some((deadline - current_tick) as u32)
        }
    }

    pub fn is_expired(&self, current_tick: u64) -> bool {
        if self.ttl_ticks == 0 {
            return false; // no TTL
        }
        self.picked_up_tick
            .is_some_and(|pickup| current_tick >= pickup + self.ttl_ticks as u64)
    }

    /// Check all steps against an agent's action log and document inventory.
    pub fn verify_steps(&self, log: &ActionLog, docs: Option<&DocumentInventory>) -> Vec<bool> {
        if self.steps.is_empty() {
            return vec![];
        }
        let start = self.picked_up_tick.unwrap_or(0);
        let end = if self.ttl_ticks > 0 {
            start + self.ttl_ticks as u64
        } else {
            u64::MAX
        };

        self.steps
            .iter()
            .map(|step| step.verify(log, start, end, self.id, docs))
            .collect()
    }

    pub fn all_steps_complete(&self, log: &ActionLog, docs: Option<&DocumentInventory>) -> bool {
        if self.steps.is_empty() {
            return true;
        }
        self.verify_steps(log, docs).iter().all(|v| *v)
    }
}

/// Central registry of all bounties.
#[derive(Resource, Default)]
pub struct BountyRegistry {
    pub bounties: Vec<Bounty>,
}

impl BountyRegistry {
    pub fn available(&self) -> Vec<&Bounty> {
        self.bounties
            .iter()
            .filter(|b| b.state == BountyState::Available && !b.expired)
            .collect()
    }

    pub fn claim(&mut self, bounty_id: Uuid, agent: Entity, tick: u64) -> Option<&Bounty> {
        // Enforce max 1 active bounty per agent.
        let already_has = self.bounties.iter().any(|b| {
            b.claimed_by == Some(agent) && b.state == BountyState::Claimed
        });
        if already_has {
            return None;
        }

        let bounty = self.bounties.iter_mut().find(|b| b.id == bounty_id)?;
        if bounty.state != BountyState::Available || bounty.expired {
            return None;
        }
        bounty.state = BountyState::Claimed;
        bounty.claimed_by = Some(agent);
        bounty.picked_up_tick = Some(tick);
        // Return immutable ref by re-finding
        self.bounties.iter().find(|b| b.id == bounty_id)
    }

    pub fn mark_completed(&mut self, bounty_id: Uuid) {
        if let Some(b) = self.bounties.iter_mut().find(|b| b.id == bounty_id) {
            b.state = BountyState::Completed;
        }
    }

    pub fn get(&self, bounty_id: Uuid) -> Option<&Bounty> {
        self.bounties.iter().find(|b| b.id == bounty_id)
    }

    pub fn agent_bounty(&self, agent: Entity) -> Option<&Bounty> {
        self.bounties
            .iter()
            .find(|b| b.claimed_by == Some(agent) && b.state == BountyState::Claimed)
    }
}

/// System: advance the board queue each tick.
pub fn advance_board_queue(mut boards: Query<&mut BoardQueue>) {
    for mut queue in &mut boards {
        queue.advance();
    }
}

/// System: check for expired bounties and force agents to return them.
pub fn bounty_expiry_system(
    tick: Res<TickCount>,
    mut registry: ResMut<BountyRegistry>,
) {
    for bounty in &mut registry.bounties {
        if bounty.state == BountyState::Claimed
            && !bounty.expired
            && bounty.is_expired(tick.0)
        {
            bounty.expired = true;
            tracing::info!(
                "Bounty '{}' has EXPIRED — agent must return it ({}g recycling fee)",
                bounty.description, recycle_cost(),
            );
        }
    }
}

/// Recycle cost for expired bounties.
/// Gold penalty for recycling expired bounties — now in config.rs.
pub fn recycle_cost() -> u32 { crate::config::recycle_cost() }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::action_log::{ActionEvent, ActionLog};

    fn entity(n: u32) -> Entity {
        Entity::from_raw_u32(n).unwrap()
    }

    // ---- Bounty constructors ----

    #[test]
    fn simple_bounty_defaults() {
        let id = Uuid::new_v4();
        let b = Bounty::simple(
            id,
            "test".into(),
            BountyObjective::WorkAtBuilding,
            50,
            vec![(ItemType::GoldEgg, 1)],
        );
        assert_eq!(b.id, id);
        assert_eq!(b.state, BountyState::Available);
        assert!(b.claimed_by.is_none());
        assert_eq!(b.reward_gold, 50);
        assert_eq!(b.ttl_ticks, 0);
        assert!(!b.expired);
        assert!(b.steps.is_empty());
        assert_eq!(b.claim_items.len(), 1);
    }

    #[test]
    fn contract_bounty_defaults() {
        let id = Uuid::new_v4();
        let steps = vec![BountyStep {
            description: "spend gold".into(),
            condition: StepCondition::SpendGold {
                building: "cafe".into(),
                amount: 10,
            },
        }];
        let b = Bounty::contract(
            id,
            "contract".into(),
            BountyObjective::WorkAtBuilding,
            100,
            5,
            20,
            steps,
        );
        assert_eq!(b.id, id);
        assert_eq!(b.state, BountyState::Available);
        assert!(b.claimed_by.is_none());
        assert_eq!(b.created_tick, 5);
        assert_eq!(b.ttl_ticks, 20);
        assert_eq!(b.steps.len(), 1);
        assert!(b.claim_items.is_empty());
    }

    // ---- TTL / expiry ----

    #[test]
    fn no_ttl_never_expires() {
        let b = Bounty::simple(
            Uuid::new_v4(),
            "no ttl".into(),
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        );
        assert!(!b.is_expired(0));
        assert!(!b.is_expired(999_999));
        assert_eq!(b.ticks_remaining(999_999), Some(u32::MAX));
    }

    #[test]
    fn ttl_not_expired_before_deadline() {
        let mut b = Bounty::contract(
            Uuid::new_v4(),
            "ttl".into(),
            BountyObjective::WorkAtBuilding,
            10,
            0,
            100,
            vec![],
        );
        b.picked_up_tick = Some(10);
        assert!(!b.is_expired(50));
        assert_eq!(b.ticks_remaining(50), Some(60));
    }

    #[test]
    fn ttl_expired_at_deadline() {
        let mut b = Bounty::contract(
            Uuid::new_v4(),
            "ttl".into(),
            BountyObjective::WorkAtBuilding,
            10,
            0,
            100,
            vec![],
        );
        b.picked_up_tick = Some(10);
        assert!(b.is_expired(110));
        assert_eq!(b.ticks_remaining(110), None);
    }

    #[test]
    fn ttl_expired_past_deadline() {
        let mut b = Bounty::contract(
            Uuid::new_v4(),
            "ttl".into(),
            BountyObjective::WorkAtBuilding,
            10,
            0,
            100,
            vec![],
        );
        b.picked_up_tick = Some(10);
        assert!(b.is_expired(200));
        assert_eq!(b.ticks_remaining(200), None);
    }

    #[test]
    fn ticks_remaining_none_when_not_picked_up() {
        let b = Bounty::contract(
            Uuid::new_v4(),
            "ttl".into(),
            BountyObjective::WorkAtBuilding,
            10,
            0,
            100,
            vec![],
        );
        // picked_up_tick is None → ticks_remaining returns None
        assert_eq!(b.ticks_remaining(50), None);
    }

    // ---- BountyRegistry claim ----

    #[test]
    fn claim_sets_state_and_agent() {
        let id = Uuid::new_v4();
        let mut reg = BountyRegistry::default();
        reg.bounties.push(Bounty::simple(
            id,
            "test".into(),
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        ));
        let agent = entity(1);
        let result = reg.claim(id, agent, 5);
        assert!(result.is_some());
        let b = reg.get(id).unwrap();
        assert_eq!(b.state, BountyState::Claimed);
        assert_eq!(b.claimed_by, Some(agent));
        assert_eq!(b.picked_up_tick, Some(5));
    }

    #[test]
    fn max_one_bounty_per_agent() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut reg = BountyRegistry::default();
        reg.bounties.push(Bounty::simple(
            id1,
            "first".into(),
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        ));
        reg.bounties.push(Bounty::simple(
            id2,
            "second".into(),
            BountyObjective::WorkAtBuilding,
            20,
            vec![],
        ));
        let agent = entity(1);
        assert!(reg.claim(id1, agent, 0).is_some());
        assert!(reg.claim(id2, agent, 1).is_none());
    }

    #[test]
    fn claim_unavailable_bounty_fails() {
        let id = Uuid::new_v4();
        let mut reg = BountyRegistry::default();
        let mut b = Bounty::simple(
            id,
            "taken".into(),
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        );
        b.state = BountyState::Claimed;
        b.claimed_by = Some(entity(1));
        reg.bounties.push(b);

        let agent2 = entity(2);
        assert!(reg.claim(id, agent2, 0).is_none());
    }

    #[test]
    fn claim_expired_bounty_fails() {
        let id = Uuid::new_v4();
        let mut reg = BountyRegistry::default();
        let mut b = Bounty::simple(
            id,
            "expired".into(),
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        );
        b.expired = true;
        reg.bounties.push(b);
        assert!(reg.claim(id, entity(1), 0).is_none());
    }

    // ---- BoardQueue ----

    #[test]
    fn board_queue_try_interact_grants_access() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        assert!(q.try_interact(a));
        assert_eq!(q.interacting, Some(a));
    }

    #[test]
    fn board_queue_try_interact_same_agent_returns_true() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        assert!(q.try_interact(a));
        assert!(q.try_interact(a));
    }

    #[test]
    fn board_queue_mutual_exclusion() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        let b = entity(2);
        assert!(q.try_interact(a));
        assert!(!q.try_interact(b));
    }

    #[test]
    fn board_queue_advance_promotes_next_waiter() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        let b = entity(2);
        q.try_interact(a);
        q.join_queue(b);
        q.leave(a);
        let next = q.advance();
        assert_eq!(next, Some(b));
        assert_eq!(q.interacting, Some(b));
    }

    #[test]
    fn board_queue_advance_noop_when_occupied() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        let b = entity(2);
        q.try_interact(a);
        q.join_queue(b);
        let next = q.advance();
        assert_eq!(next, None);
    }

    #[test]
    fn board_queue_advance_empty() {
        let mut q = BoardQueue::default();
        assert_eq!(q.advance(), None);
    }

    #[test]
    fn board_queue_leave_clears_interacting() {
        // Regression: agent left board without calling leave(), blocking queue.
        let mut q = BoardQueue::default();
        let a = entity(1);
        let b = entity(2);
        q.try_interact(a);
        q.join_queue(b);
        // Agent A walks away — leave should clear them.
        q.leave(a);
        assert_eq!(q.interacting, None);
        // Now B can advance.
        let next = q.advance();
        assert_eq!(next, Some(b));
    }

    #[test]
    fn board_queue_leave_removes_from_waiting() {
        let mut q = BoardQueue::default();
        let a = entity(1);
        let b = entity(2);
        q.try_interact(a);
        q.join_queue(b);
        q.leave(b);
        assert!(!q.waiting.contains(&b));
    }

    #[test]
    fn bounty_claim_sets_state_and_agent() {
        let mut reg = BountyRegistry::default();
        let id = Uuid::new_v4();
        reg.bounties.push(Bounty::simple(
            id, "test".into(), BountyObjective::WorkAtBuilding, 5, vec![],
        ));
        let agent = entity(1);
        let bounty = reg.claim(id, agent, 100);
        assert!(bounty.is_some());
        let b = reg.get(id).unwrap();
        assert_eq!(b.state, BountyState::Claimed);
        assert_eq!(b.claimed_by, Some(agent));
    }

    #[test]
    fn bounty_double_claim_fails() {
        let mut reg = BountyRegistry::default();
        let id = Uuid::new_v4();
        reg.bounties.push(Bounty::simple(
            id, "test".into(), BountyObjective::WorkAtBuilding, 5, vec![],
        ));
        let a = entity(1);
        let b = entity(2);
        assert!(reg.claim(id, a, 100).is_some());
        assert!(reg.claim(id, b, 100).is_none());
    }

    #[test]
    fn bounty_available_excludes_claimed() {
        let mut reg = BountyRegistry::default();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        reg.bounties.push(Bounty::simple(
            id1, "claimed".into(), BountyObjective::WorkAtBuilding, 5, vec![],
        ));
        reg.bounties.push(Bounty::simple(
            id2, "available".into(), BountyObjective::WorkAtBuilding, 5, vec![],
        ));
        reg.claim(id1, entity(1), 100);
        let avail = reg.available();
        assert_eq!(avail.len(), 1);
        assert_eq!(avail[0].id, id2);
    }

    // ---- Step verification: SpendGold ----

    #[test]
    fn spend_gold_step_passes_when_sufficient() {
        let step = BountyStep {
            description: "spend gold".into(),
            condition: StepCondition::SpendGold {
                building: "cafe".into(),
                amount: 10,
            },
        };
        let bounty_id = Uuid::new_v4();
        let mut log = ActionLog::default();
        log.log(5, ActionEvent::GoldSpent { amount: 6, building: "cafe".into() });
        log.log(7, ActionEvent::GoldSpent { amount: 5, building: "cafe".into() });
        assert!(step.verify(&log, 0, 10, bounty_id, None));
    }

    #[test]
    fn spend_gold_step_fails_when_insufficient() {
        let step = BountyStep {
            description: "spend gold".into(),
            condition: StepCondition::SpendGold {
                building: "cafe".into(),
                amount: 10,
            },
        };
        let bounty_id = Uuid::new_v4();
        let mut log = ActionLog::default();
        log.log(5, ActionEvent::GoldSpent { amount: 4, building: "cafe".into() });
        assert!(!step.verify(&log, 0, 10, bounty_id, None));
    }

    #[test]
    fn spend_gold_wrong_building_not_counted() {
        let step = BountyStep {
            description: "spend gold".into(),
            condition: StepCondition::SpendGold {
                building: "cafe".into(),
                amount: 5,
            },
        };
        let bounty_id = Uuid::new_v4();
        let mut log = ActionLog::default();
        log.log(5, ActionEvent::GoldSpent { amount: 10, building: "market".into() });
        assert!(!step.verify(&log, 0, 10, bounty_id, None));
    }

    #[test]
    fn spend_gold_outside_tick_range_ignored() {
        let step = BountyStep {
            description: "spend gold".into(),
            condition: StepCondition::SpendGold {
                building: "cafe".into(),
                amount: 5,
            },
        };
        let bounty_id = Uuid::new_v4();
        let mut log = ActionLog::default();
        log.log(20, ActionEvent::GoldSpent { amount: 10, building: "cafe".into() });
        assert!(!step.verify(&log, 0, 10, bounty_id, None));
    }
}
