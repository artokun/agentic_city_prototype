use bevy::prelude::*;
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::agents::action_log::{ActionEvent, ActionLog};
use crate::agents::actions::ActionTimer;
use crate::agents::components::*;
use crate::agents::conversation::{ActiveConversation, ConversationLog};
use crate::agents::event_log::AgentEventLog;
use crate::agents::needs::Needs;
use crate::agents::perception::{KnownLocations, Tracking, Vision};
use crate::agents::social::Relationships;
use crate::agents::token_tracking::{AgentCost, ContextWindow};
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::*;
use crate::world::hospital::Incapacitated;
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
        Self {
            sender: Arc::new(sender),
        }
    }
}

pub fn broadcast_state(
    broadcast_tx: Res<BroadcastTx>,
    tick: Res<TickCount>,
    agents: Query<(
        Entity,
        &AgentId,
        &AgentName,
        &GridPos,
        &AgentAnimation,
        &ThoughtBubble,
        &Inventory,
        &AgentGoal,
        &Needs,
        &Relationships,
        &Vision,
        &Tracking,
        &KnownLocations,
        Option<&ActionTimer>,
        Option<&ActiveConversation>,
    )>,
    agent_extras: Query<(
        Option<&ConversationLog>,
        Option<&BusinessCards>,
        Option<&ContextWindow>,
        Option<&AgentCost>,
        Option<&crate::items::DocumentInventory>,
        Option<&crate::items::ContainedItems>,
    )>,
    item_entities: Query<(
        &crate::items::ItemKind,
        Option<&crate::items::ItemName>,
        Option<&crate::items::BountyTokenInfo>,
    )>,
    structures: Query<
        (
            &StructureId,
            &GridPos,
            &SpriteType,
            Option<&Interactable>,
            &Inventory,
            Option<&crate::items::DocumentInventory>,
            Option<&crate::items::ContainedItems>,
            &Entrance,
        ),
        Without<AgentName>,
    >,
    boards: Query<&BountyTokenStore, With<BountyBoard>>,
    board_queues: Query<&BoardQueue, With<BountyBoard>>,
    agent_names: Query<&AgentName>,
    event_log: Res<AgentEventLog>,
) {
    let Some(store) = boards.iter().next() else {
        return;
    };
    let bytes = serializer::serialize_world(
        &tick,
        &agents,
        &agent_extras,
        &item_entities,
        &structures,
        store,
        &board_queues,
        &agent_names,
        &event_log,
    );
    let _ = broadcast_tx.sender.send(bytes);
}

/// Bevy resource holding the shared JSON world state for GM queries.
#[derive(Resource)]
pub struct WorldStateJsonHolder(pub std::sync::Arc<std::sync::RwLock<String>>);

fn serialize_action_event(event: &ActionEvent) -> serde_json::Value {
    match event {
        ActionEvent::GoldSpent { amount, building } => serde_json::json!({
            "kind": "gold_spent",
            "amount": amount,
            "building": building,
        }),
        ActionEvent::ServiceUsed { service, building } => serde_json::json!({
            "kind": "service_used",
            "service": service,
            "building": building,
        }),
        ActionEvent::DocumentProduced { title, bounty_id } => serde_json::json!({
            "kind": "document_produced",
            "title": title,
            "bounty_id": bounty_id.to_string(),
        }),
        ActionEvent::BountyPickedUp { bounty_id } => serde_json::json!({
            "kind": "bounty_picked_up",
            "bounty_id": bounty_id.to_string(),
        }),
        ActionEvent::BountyReturned { bounty_id } => serde_json::json!({
            "kind": "bounty_returned",
            "bounty_id": bounty_id.to_string(),
        }),
        ActionEvent::EnteredBuilding { building } => serde_json::json!({
            "kind": "entered_building",
            "building": building,
        }),
        ActionEvent::WebSearched { query } => serde_json::json!({
            "kind": "web_searched",
            "query": query,
        }),
        ActionEvent::ChattedWith { agent } => serde_json::json!({
            "kind": "chatted_with",
            "agent": agent,
        }),
        ActionEvent::Inspected { target } => serde_json::json!({
            "kind": "inspected",
            "target": target,
        }),
        ActionEvent::ItemReceived { item, count } => serde_json::json!({
            "kind": "item_received",
            "item": item,
            "count": count,
        }),
        ActionEvent::ItemGiven { item, count } => serde_json::json!({
            "kind": "item_given",
            "item": item,
            "count": count,
        }),
    }
}

/// System: serialize world state as JSON for GM query endpoint.
/// Runs every tick so GM reviews never inspect stale submission state.
pub fn update_world_state_json(
    tick: Res<TickCount>,
    agents: Query<
        (
            &AgentName,
            &GridPos,
            &AgentGoal,
            &Needs,
            &Inventory,
            &crate::agents::components::BusinessCards,
            &ContextWindow,
            &AgentCost,
            &ActionLog,
            Option<&crate::items::DocumentInventory>,
            &KnownLocations,
            &Relationships,
            Option<&Incapacitated>,
        ),
        bevy::prelude::With<AgentId>,
    >,
    structures: Query<
        (
            &SpriteType,
            &GridPos,
            &Inventory,
            Option<&crate::items::DocumentInventory>,
        ),
        (With<StructureId>, Without<AgentName>),
    >,
    boards_json: Query<
        (&BountyTokenStore, &crate::world::bounty::BountyDropbox),
        With<BountyBoard>,
    >,
    event_log: Res<AgentEventLog>,
    holder: Res<WorldStateJsonHolder>,
    library: Res<crate::world::bounty::Library>,
    agent_names_json: Query<&AgentName>,
) {
    let Some((bounty_registry, board_dropbox)) = boards_json.iter().next() else {
        return;
    };

    let agents_json: Vec<serde_json::Value> = agents.iter().map(|(name, pos, goal, needs, inv, cards, ctx_window, agent_cost, action_log, docs, known_locs, rels, incap)| {
        let items: std::collections::HashMap<String, u32> = inv.items.iter()
            .map(|(k, v)| (k.to_string(), *v)).collect();
        let documents: Vec<serde_json::Value> = docs
            .map(|docs| {
                docs.documents
                    .iter()
                    .map(|(title, content)| {
                        serde_json::json!({
                            "title": title,
                            "content_length": content.len(),
                            "content": content,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let action_log_tail: Vec<serde_json::Value> = action_log
            .entries
            .iter()
            .rev()
            .take(50)
            .map(|entry| {
                serde_json::json!({
                    "tick": entry.tick,
                    "event": serialize_action_event(&entry.event),
                })
            })
            .collect();
        let locations_json: Vec<serde_json::Value> = known_locs.locations.values().map(|loc| {
            serde_json::json!({
                "name": loc.name,
                "position": { "x": loc.pos.x, "y": loc.pos.y },
                "entrance": { "x": loc.entrance.x, "y": loc.entrance.y },
            })
        }).collect();
        let relationships_json: Vec<serde_json::Value> = rels.known.values().map(|mem| {
            serde_json::json!({
                "name": mem.name,
                "friendship": mem.friendship,
                "last_known_goal": mem.last_known_goal,
            })
        }).collect();
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
            "tokens_used": ctx_window.tokens_used,
            "context_limit": ctx_window.context_limit,
            "total_cost_usd": agent_cost.total_cost_usd,
            "documents": documents,
            "action_log_tail": action_log_tail,
            "known_locations": locations_json,
            "relationships": relationships_json,
            "incapacitated": incap.is_some(),
        })
    }).collect();

    let structures_json: Vec<serde_json::Value> = structures
        .iter()
        .map(|(sprite, pos, inv, docs)| {
            let items: std::collections::HashMap<String, u32> =
                inv.items.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            let documents: Vec<serde_json::Value> = docs
                .map(|docs| {
                    docs.documents
                        .iter()
                        .map(|(title, content)| {
                            serde_json::json!({
                                "title": title,
                                "content_length": content.len(),
                                "content": content,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "name": sprite.0,
                "position": { "x": pos.x, "y": pos.y },
                "inventory": items,
                "documents": documents,
            })
        })
        .collect();

    let bounties_json: Vec<serde_json::Value> = bounty_registry
        .tokens
        .values()
        .map(|b| {
            let claimed_by = b
                .claimed_by
                .and_then(|entity| agent_names_json.get(entity).ok())
                .map(|name| name.0.clone());
            serde_json::json!({
                "id": b.id.to_string(),
                "description": b.description,
                "reward_gold": b.reward_gold,
                "state": format!("{:?}", b.state),
                "objective": format!("{:?}", b.objective),
                "hidden_criteria": b.hidden_criteria,
                "claimed_by": claimed_by,
                "picked_up_tick": b.picked_up_tick,
                "ttl_ticks": b.ttl_ticks,
                "expired": b.expired,
            })
        })
        .collect();

    let recent_logs: Vec<serde_json::Value> = event_log
        .entries
        .iter()
        .rev()
        .take(50)
        .map(|e| {
            serde_json::json!({
                "tick": e.tick,
                "agent": e.agent,
                "kind": e.kind.as_str(),
                "text": e.text,
            })
        })
        .collect();

    // Dropbox contents — keyed by agent name for GM visibility.
    let dropbox_json: Vec<serde_json::Value> =
        board_dropbox
            .slots
            .iter()
            .map(|(agent_entity, slot)| {
                let agent_name = agent_names_json
                    .get(*agent_entity)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|_| format!("entity_{}", agent_entity.to_bits()));
                let items: Vec<serde_json::Value> = slot.items.iter().map(|(item, count)| {
            serde_json::json!({ "item": item.to_string(), "count": count })
        }).collect();
                let docs: Vec<serde_json::Value> = slot
                    .documents
                    .iter()
                    .map(|(title, content)| {
                        serde_json::json!({
                            "title": title,
                            "content_length": content.len(),
                            "content": content,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "agent": agent_name,
                    "bounty_token_id": slot.bounty_token_id.map(|id| id.to_string()),
                    "items": items,
                    "documents": docs,
                })
            })
            .collect();

    let world = serde_json::json!({
        "tick": tick.0,
        "agents": agents_json,
        "structures": structures_json,
        "bounties": bounties_json,
        "dropbox": dropbox_json,
        "library_document_count": library.documents.len(),
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

/// System: serialize library catalog as JSON for the UI endpoint.
pub fn update_library_json(
    library: Res<crate::world::bounty::Library>,
    holder: Res<super::LibraryJsonArc>,
) {
    if !library.is_changed() {
        return;
    }
    let entries: Vec<serde_json::Value> = library
        .documents
        .iter()
        .map(|doc| {
            serde_json::json!({
                "title": doc.title,
                "author": doc.author,
                "bounty": doc.bounty_description,
                "tick": doc.tick,
                "content": doc.content,
            })
        })
        .collect();
    let json_str = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
    let mut guard = holder.0.write().unwrap();
    *guard = json_str;
}
