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
        Entity, &AgentId, &AgentName, &GridPos, &AgentAnimation, &ThoughtBubble,
        &Inventory, &AgentGoal, &Needs, &Relationships, &Vision, &Tracking, &KnownLocations,
        Option<&ActionTimer>, Option<&ActiveConversation>,
    )>,
    agent_extras: Query<(Option<&ConversationLog>, Option<&BusinessCards>)>,
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
        &tick, &agents, &agent_extras, &structures, &bounty_registry, &board_queues, &agent_names, &event_log,
    );
    let _ = broadcast_tx.sender.send(bytes);
}

/// Bevy resource holding the shared JSON world state for GM queries.
#[derive(Resource)]
pub struct WorldStateJsonHolder(pub std::sync::Arc<std::sync::RwLock<String>>);

/// System: serialize world state as JSON for GM query endpoint.
/// Runs every 10 ticks to avoid overhead.
pub fn update_world_state_json(
    tick: Res<TickCount>,
    agents: Query<(
        &AgentName, &GridPos, &AgentGoal, &Needs, &Inventory,
        &crate::agents::components::BusinessCards,
    ), bevy::prelude::With<AgentId>>,
    structures: Query<
        (&SpriteType, &GridPos, &Inventory),
        (With<StructureId>, Without<AgentName>),
    >,
    bounty_registry: Res<BountyRegistry>,
    event_log: Res<AgentEventLog>,
    holder: Res<WorldStateJsonHolder>,
) {
    if tick.0 % 10 != 0 { return; }

    let agents_json: Vec<serde_json::Value> = agents.iter().map(|(name, pos, goal, needs, inv, cards)| {
        let items: std::collections::HashMap<String, u32> = inv.items.iter()
            .map(|(k, v)| (k.to_string(), *v)).collect();
        // Get documents from DocumentInventory if available.
        // (DocumentInventory is not in this query — we'd need a separate lookup.
        //  For now, documents are tracked via the Document item count.)
        serde_json::json!({
            "name": name.0,
            "position": { "x": pos.x, "y": pos.y },
            "goal": format!("{:?}", goal),
            "needs": { "energy": needs.energy, "hunger": needs.hunger, "boredom": needs.boredom },
            "inventory": items,
            "gold": inv.count(crate::items::ItemType::GoldCoin),
            "gold_debt": inv.gold_debt,
            "contacts": cards.contacts.keys().collect::<Vec<_>>(),
            "cards_remaining": cards.cards_remaining,
        })
    }).collect();

    let structures_json: Vec<serde_json::Value> = structures.iter().map(|(sprite, pos, inv)| {
        let items: std::collections::HashMap<String, u32> = inv.items.iter()
            .map(|(k, v)| (k.to_string(), *v)).collect();
        serde_json::json!({
            "name": sprite.0,
            "position": { "x": pos.x, "y": pos.y },
            "inventory": items,
        })
    }).collect();

    let bounties_json: Vec<serde_json::Value> = bounty_registry.bounties.iter().map(|b| {
        serde_json::json!({
            "id": b.id.to_string(),
            "description": b.description,
            "reward_gold": b.reward_gold,
            "state": format!("{:?}", b.state),
            "objective": format!("{:?}", b.objective),
            "hidden_criteria": b.hidden_criteria,
        })
    }).collect();

    let recent_logs: Vec<serde_json::Value> = event_log.entries.iter().rev().take(50).map(|e| {
        serde_json::json!({
            "tick": e.tick,
            "agent": e.agent,
            "kind": e.kind.as_str(),
            "text": e.text,
        })
    }).collect();

    let world = serde_json::json!({
        "tick": tick.0,
        "agents": agents_json,
        "structures": structures_json,
        "bounties": bounties_json,
        "recent_logs": recent_logs,
    });

    let json_str = serde_json::to_string_pretty(&world).unwrap_or_default();

    // Write to shared state.
    {
        let state = holder.0.clone();
        let mut guard = state.write().unwrap();
        *guard = json_str;
    }
}
