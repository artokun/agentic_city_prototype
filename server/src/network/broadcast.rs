use bevy::prelude::*;
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::agents::actions::ActionTimer;
use crate::agents::components::*;
use crate::agents::conversation::{ActiveConversation, ConversationLog};
use crate::agents::needs::Needs;
use crate::agents::event_log::AgentEventLog;
use crate::agents::perception::{KnownLocations, Tracking, Vision};
use crate::agents::social::Relationships;
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::*;
use crate::world::map::GridPos;
use crate::world::structures::*;

use super::serializer;

#[derive(Resource)]
pub struct BroadcastTx {
    pub sender: Arc<broadcast::Sender<Bytes>>,
}

impl Default for BroadcastTx {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(64);
        Self { sender: Arc::new(sender) }
    }
}

pub fn broadcast_state(
    broadcast_tx: Res<BroadcastTx>,
    tick: Res<TickCount>,
    agents: Query<(
        &AgentId, &AgentName, &GridPos, &AgentAnimation, &ThoughtBubble,
        &Inventory, &AgentGoal, &Needs, &Relationships, &Vision, &Tracking, &KnownLocations,
        Option<&ActionTimer>, Option<&ActiveConversation>, Option<&ConversationLog>,
    )>,
    structures: Query<
        (&StructureId, &GridPos, &SpriteType, Option<&Interactable>, &Inventory, &Entrance),
        Without<AgentName>,
    >,
    bounty_registry: Res<BountyRegistry>,
    board_queues: Query<&BoardQueue, With<BountyBoard>>,
    agent_names: Query<&AgentName>,
    event_log: Res<AgentEventLog>,
) {
    let bytes = serializer::serialize_world(
        &tick, &agents, &structures, &bounty_registry, &board_queues, &agent_names, &event_log,
    );
    let _ = broadcast_tx.sender.send(bytes);
}
