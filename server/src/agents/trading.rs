use bevy::prelude::*;

use crate::agents::components::AgentName;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;

/// Parse an item name string into an ItemType.
pub fn parse_item_type(s: &str) -> Option<ItemType> {
    match s {
        "gold_coin" => Some(ItemType::GoldCoin),
        "gold_egg" => Some(ItemType::GoldEgg),
        "coffee" => Some(ItemType::Coffee),
        "muffin" => Some(ItemType::Muffin),
        "rations" => Some(ItemType::Rations),
        "sandwich" => Some(ItemType::Sandwich),
        "soup" => Some(ItemType::Soup),
        "paycheck" => return None, // non-tradeable
        "document" => Some(ItemType::Document),
        "coffee_beans" => Some(ItemType::CoffeeBeans),
        "flour" => Some(ItemType::Flour),
        "raw_meat" => Some(ItemType::RawMeat),
        _ => None,
    }
}

/// Parse a comma-separated list of item names into a Vec<ItemType>.
pub fn parse_item_list(s: &str) -> Result<Vec<ItemType>, String> {
    let mut items = Vec::new();
    for name in s.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match parse_item_type(name) {
            Some(item) => items.push(item),
            None => return Err(format!("Unknown item: '{}'", name)),
        }
    }
    Ok(items)
}

/// Attached to the proposer entity. Tracks a pending trade between two agents.
#[derive(Component, Debug, Clone)]
pub struct TradeProposal {
    pub proposer: Entity,
    pub responder: Entity,
    pub offered_items: Vec<ItemType>,
    pub requested_items: Vec<ItemType>,
    pub proposer_accepted: bool,
    pub responder_accepted: bool,
}

/// System: when both sides accept, execute the item swap and clean up.
pub fn trade_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    proposals: Query<(Entity, &TradeProposal)>,
    mut inventories: Query<(&AgentName, &mut Inventory)>,
) {
    for (proposal_entity, trade) in &proposals {
        if !trade.proposer_accepted || !trade.responder_accepted {
            continue;
        }

        // Validate both agents still exist and have the items.
        let proposer_ok = {
            if let Ok((_, inv)) = inventories.get(trade.proposer) {
                trade.offered_items.iter().all(|item| inv.has(*item, 1))
            } else {
                false
            }
        };

        let responder_ok = {
            if let Ok((_, inv)) = inventories.get(trade.responder) {
                trade.requested_items.iter().all(|item| inv.has(*item, 1))
            } else {
                false
            }
        };

        if !proposer_ok || !responder_ok {
            // One side can't fulfill — cancel the trade.
            let proposer_name = inventories
                .get(trade.proposer)
                .map(|(n, _)| n.0.clone())
                .unwrap_or_else(|_| "unknown".into());
            let responder_name = inventories
                .get(trade.responder)
                .map(|(n, _)| n.0.clone())
                .unwrap_or_else(|_| "unknown".into());

            event_log.push(LogEvent {
                tick: tick.0,
                agent: proposer_name.clone(),
                kind: LogKind::System,
                text: format!(
                    "Trade with {} cancelled: insufficient items.",
                    responder_name
                ),
            });

            commands.entity(proposal_entity).despawn();
            continue;
        }

        // Execute the swap.
        // Remove offered items from proposer.
        let proposer_name;
        {
            let (name, mut inv) = inventories.get_mut(trade.proposer).unwrap();
            proposer_name = name.0.clone();
            for item in &trade.offered_items {
                inv.remove(*item, 1);
            }
            // Add requested items to proposer.
            for item in &trade.requested_items {
                inv.add(*item, 1);
            }
        }

        // Remove requested items from responder, add offered items.
        let responder_name;
        {
            let (name, mut inv) = inventories.get_mut(trade.responder).unwrap();
            responder_name = name.0.clone();
            for item in &trade.requested_items {
                inv.remove(*item, 1);
            }
            for item in &trade.offered_items {
                inv.add(*item, 1);
            }
        }

        // Log the trade for both agents.
        let offered_str: Vec<String> = trade.offered_items.iter().map(|i| i.to_string()).collect();
        let requested_str: Vec<String> = trade
            .requested_items
            .iter()
            .map(|i| i.to_string())
            .collect();

        event_log.push(LogEvent {
            tick: tick.0,
            agent: proposer_name.clone(),
            kind: LogKind::Action,
            text: format!(
                "Traded [{}] to {} for [{}].",
                offered_str.join(", "),
                responder_name,
                requested_str.join(", "),
            ),
        });

        event_log.push(LogEvent {
            tick: tick.0,
            agent: responder_name.clone(),
            kind: LogKind::Action,
            text: format!(
                "Traded [{}] to {} for [{}].",
                requested_str.join(", "),
                proposer_name,
                offered_str.join(", "),
            ),
        });

        tracing::info!(
            "[TRADE] {} <-> {}: [{}] for [{}]",
            proposer_name,
            responder_name,
            offered_str.join(", "),
            requested_str.join(", "),
        );

        // Remove the proposal entity.
        commands.entity(proposal_entity).despawn();
    }
}
