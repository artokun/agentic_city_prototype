use bevy::prelude::*;
use uuid::Uuid;

use crate::world::map::GridPos;

/// Append-only log of actions an agent has performed. Used for bounty verification.
#[derive(Component, Default, Debug)]
pub struct ActionLog {
    pub entries: Vec<LogEntry>,
}

impl ActionLog {
    pub fn log(&mut self, tick: u64, event: ActionEvent) {
        self.entries.push(LogEntry { tick, event });
    }

    /// Check if an event matching the predicate occurred between start and end ticks.
    pub fn has_between(&self, start: u64, end: u64, pred: impl Fn(&ActionEvent) -> bool) -> bool {
        self.entries
            .iter()
            .any(|e| e.tick >= start && e.tick <= end && pred(&e.event))
    }

    /// Count events matching predicate between ticks.
    pub fn count_between(
        &self,
        start: u64,
        end: u64,
        pred: impl Fn(&ActionEvent) -> bool,
    ) -> usize {
        self.entries
            .iter()
            .filter(|e| e.tick >= start && e.tick <= end && pred(&e.event))
            .count()
    }

    /// Trim old entries to keep log manageable.
    pub fn trim(&mut self, keep_after_tick: u64) {
        self.entries.retain(|e| e.tick >= keep_after_tick);
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub tick: u64,
    pub event: ActionEvent,
}

#[derive(Debug, Clone)]
pub enum ActionEvent {
    /// Spent gold at a building.
    GoldSpent { amount: u32, building: String },
    /// Used a service at a building.
    ServiceUsed { service: String, building: String },
    /// Produced a document.
    DocumentProduced { title: String, bounty_id: Uuid },
    /// Picked up a bounty from the board.
    BountyPickedUp { bounty_id: Uuid },
    /// Returned a bounty to the board.
    BountyReturned { bounty_id: Uuid },
    /// Entered a building.
    EnteredBuilding { building: String },
    /// Searched (web fetch / Google).
    WebSearched { query: String },
    /// Chatted with another agent.
    ChattedWith { agent: String },
    /// Inspected an entity.
    Inspected { target: String },
    /// Item added to inventory.
    ItemReceived { item: String, count: u32 },
    /// Item removed from inventory.
    ItemGiven { item: String, count: u32 },
}
