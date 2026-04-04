use bevy::prelude::*;
use std::collections::VecDeque;
use uuid::Uuid;

use crate::items::ItemType;
use crate::world::map::GridPos;

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
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BountyObjective {
    /// Hide the given item inside any structure's inventory.
    HideItem(ItemType),
    /// Find the given item (search structures, pick it up).
    FindItem(ItemType),
    /// Work at the building that posted this job.
    WorkAtBuilding,
}

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
            .filter(|b| b.state == BountyState::Available)
            .collect()
    }

    pub fn claim(&mut self, bounty_id: Uuid, agent: Entity) -> Option<&Bounty> {
        let bounty = self.bounties.iter_mut().find(|b| b.id == bounty_id)?;
        if bounty.state != BountyState::Available {
            return None;
        }
        bounty.state = BountyState::Claimed;
        bounty.claimed_by = Some(agent);
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
            .find(|b| b.claimed_by == Some(agent) && b.state != BountyState::Available)
    }
}

/// System: advance the board queue each tick.
pub fn advance_board_queue(mut boards: Query<&mut BoardQueue>) {
    for mut queue in &mut boards {
        queue.advance();
    }
}
