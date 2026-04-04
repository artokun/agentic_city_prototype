use bevy::prelude::*;
use uuid::Uuid;

use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};
use crate::world::structures::SpriteType;

/// Gold reserve a building has earned from customers.
#[derive(Component, Default, Debug)]
pub struct GoldReserve(pub i32);

/// What items this building sells and at what stock levels.
#[derive(Component, Debug)]
pub struct RetailConfig {
    pub stock: Vec<StockItem>,
}

#[derive(Debug, Clone)]
pub struct StockItem {
    pub item: ItemType,
    pub max: u32,
    /// Below this level, auto-generate a restock bounty.
    pub reorder_at: u32,
    /// How many to order in a restock batch.
    pub reorder_qty: u32,
}

/// Marks a building as a wholesale supplier (warehouse).
#[derive(Component)]
pub struct Warehouse;

/// System: check retail buildings for low stock, auto-create restock bounties.
pub fn auto_restock_system(
    tick: Res<TickCount>,
    retailers: Query<(Entity, &SpriteType, &Inventory, &RetailConfig, &GoldReserve)>,
    mut bounty_registry: ResMut<BountyRegistry>,
) {
    // Only check every 50 ticks (5 seconds).
    if tick.0 % 50 != 0 {
        return;
    }

    for (_entity, sprite, inv, config, gold_reserve) in &retailers {
        for stock in &config.stock {
            let current = inv.count(stock.item);

            if current > stock.reorder_at {
                continue;
            }

            // Check if there's already a pending restock bounty for this item at this building.
            let already_pending = bounty_registry.bounties.iter().any(|b| {
                (b.state == BountyState::Available || b.state == BountyState::Claimed)
                    && b.description.contains(&sprite.0)
                    && b.description.contains(&stock.item.to_string())
            });

            if already_pending {
                continue;
            }

            // Calculate cost: wholesale price for the reorder quantity.
            let wholesale_cost = stock.item.wholesale_price().map(|(gold, units)| {
                let batches = (stock.reorder_qty + units - 1) / units; // ceil division
                batches * gold
            }).unwrap_or(1);

            // Reward = wholesale cost + 1g profit margin for the delivery agent.
            let reward = wholesale_cost + 1;

            // Can the building afford this bounty?
            if gold_reserve.0 < reward as i32 {
                continue;
            }

            let bounty = Bounty {
                id: Uuid::new_v4(),
                description: format!(
                    "Restock {} {} at {} (buy from warehouse, deliver here)",
                    stock.reorder_qty, stock.item, sprite.0,
                ),
                objective: BountyObjective::RestockDelivery {
                    item: stock.item,
                    quantity: stock.reorder_qty,
                    destination: sprite.0.clone(),
                },
                reward_gold: reward,
                state: BountyState::Available,
                claimed_by: None,
                claim_items: vec![],
            };

            tracing::info!(
                "Auto-restock: {} needs {} {} (reward: {}g, reserve: {}g)",
                sprite.0, stock.reorder_qty, stock.item, reward, gold_reserve.0,
            );

            bounty_registry.bounties.push(bounty);
        }
    }
}

/// System: when an agent uses a paid service, transfer gold to the building's reserve.
pub fn building_revenue_system(
    // This is handled inline by the action_timer_system when gold_cost > 0.
    // We just need a system that deducts the bounty reward from the building's reserve
    // when a restock bounty is claimed.
) {
    // Placeholder — revenue collection happens in the action timer.
    // The reserve deduction for bounties needs to happen when the bounty is created.
}
