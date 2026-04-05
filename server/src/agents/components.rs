use bevy::prelude::*;
use uuid::Uuid;

use crate::agents::action_log::ActionLog;
use crate::agents::mailbox::Mailbox;
use crate::agents::needs::Needs;
use crate::agents::perception::{InspectionLog, KnownLocations, Tracking, Vision};
use crate::agents::social::Relationships;
use crate::items::Inventory;
use crate::world::map::GridPos;
use crate::world::shifts::PaycheckWallet;

#[derive(Component)]
pub struct AgentId(pub Uuid);

#[derive(Component)]
pub struct AgentName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnimState {
    #[default]
    Idle,
    Walking,
    Working,
}

#[derive(Component, Default)]
pub struct AgentAnimation(pub AnimState);

#[derive(Component, Default)]
pub struct ThoughtBubble(pub String);

/// Tiles per second.
#[derive(Component)]
pub struct Speed(pub f32);

/// Accumulates time between tile moves.
#[derive(Component)]
pub struct MoveTimer(pub Timer);

impl MoveTimer {
    pub fn from_speed(tiles_per_sec: f32) -> Self {
        Self(Timer::from_seconds(1.0 / tiles_per_sec, TimerMode::Repeating))
    }
}

/// Remaining path waypoints.
#[derive(Component)]
pub struct Path(pub std::collections::VecDeque<GridPos>);

/// What the agent is currently trying to do.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum AgentGoal {
    /// No goal — will evaluate needs and decide.
    Idle,
    /// Wandering to kill time before checking the board again.
    Wandering,
    /// Walking to the bounty board to get a bounty.
    GoingToBoard,
    /// Waiting in queue at the board.
    WaitingAtBoard,
    /// Interacting with the board (has exclusive access).
    InteractingWithBoard,
    /// Executing a bounty objective.
    ExecutingBounty(Uuid),
    /// Returning to the board to claim reward.
    ReturningToBoard(Uuid),
    /// Walking to a building to use a service.
    GoingToService { building: Entity, service: String },
    /// Performing a timed action (ActionTimer is ticking).
    PerformingAction,
    /// Working a shift at a building (ShiftWorker tracks progress).
    WorkingShift { building: Entity },
}

impl Default for AgentGoal {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Bundle)]
pub struct AgentBundle {
    pub id: AgentId,
    pub name: AgentName,
    pub pos: GridPos,
    pub animation: AgentAnimation,
    pub thought: ThoughtBubble,
    pub inventory: Inventory,
    pub needs: Needs,
    pub relationships: Relationships,
    pub vision: Vision,
    pub tracking: Tracking,
    pub inspection_log: InspectionLog,
    pub known_locations: KnownLocations,
    pub action_log: ActionLog,
    pub mailbox: Mailbox,
    pub speed: Speed,
    pub move_timer: MoveTimer,
    pub goal: AgentGoal,
    pub paycheck_wallet: PaycheckWallet,
}

impl AgentBundle {
    pub fn new(name: &str, pos: GridPos, speed: f32) -> Self {
        Self {
            id: AgentId(Uuid::new_v4()),
            name: AgentName(name.into()),
            pos,
            animation: AgentAnimation::default(),
            thought: ThoughtBubble(format!("{name} is looking around...")),
            inventory: {
                let mut inv = Inventory::default();
                inv.add(crate::items::ItemType::GoldCoin, 5); // starting gold
                inv
            },
            needs: Needs::default(),
            relationships: Relationships::default(),
            vision: Vision::default(),
            tracking: Tracking::default(),
            inspection_log: InspectionLog::default(),
            known_locations: KnownLocations::default(),
            action_log: ActionLog::default(),
            mailbox: Mailbox::default(),
            speed: Speed(speed),
            move_timer: MoveTimer::from_speed(speed),
            goal: AgentGoal::default(),
            paycheck_wallet: PaycheckWallet::default(),
        }
    }
}
