//! Agent-to-agent transactions.
//! When a customer with `WantsToBuy` is at the same building as a `ShiftWorker`,
//! the worker sells the item: removes it from building inventory, adds it to
//! the customer's inventory, and the customer pays the retail price in gold.

use bevy::prelude::*;

use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::shifts::ShiftWorker;
use crate::world::structures::{InsideBuilding, SpriteType};

/// Marks an agent as wanting to buy an item at a specific building.
/// Attached by the AI / behavior layer when the agent arrives at a service
/// building and wants to purchase something.
#[derive(Component, Debug)]
pub struct WantsToBuy {
    pub item: ItemType,
    pub from_building: Entity,
}

/// System: match customers who want to buy with shift workers at the same
/// building, then execute the sale.
pub fn transaction_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    // Customers wanting to buy something (must also be InsideBuilding).
    mut customers: Query<
        (
            Entity,
            &AgentName,
            &WantsToBuy,
            &InsideBuilding,
            &mut Inventory,
        ),
        Without<ShiftWorker>,
    >,
    // Workers on shift (must also be InsideBuilding).
    workers: Query<(Entity, &AgentName, &ShiftWorker, &InsideBuilding)>,
    // Building inventories and names.
    mut buildings: Query<(&mut Inventory, &SpriteType), (Without<AgentName>, Without<ShiftWorker>)>,
) {
    for (cust_entity, cust_name, wants, cust_inside, mut cust_inv) in &mut customers {
        let target_building = wants.from_building;

        // Customer must actually be inside the target building.
        if cust_inside.0 != target_building {
            continue;
        }

        // Find a worker at the same building.
        let worker = workers.iter().find(|(_, _, shift, w_inside)| {
            shift.building == target_building && w_inside.0 == target_building
        });

        let Some((_worker_entity, worker_name, _shift, _)) = worker else {
            continue;
        };

        // Check retail price.
        let price = wants.item.retail_price();

        // Customer must be able to afford the item.
        if price > 0 && !cust_inv.has(ItemType::GoldCoin, price) {
            event_log.push(LogEvent {
                tick: tick.0,
                agent: cust_name.0.clone(),
                kind: LogKind::Action,
                text: format!("Can't afford {} (need {}g)", wants.item, price,),
            });
            commands.entity(cust_entity).remove::<WantsToBuy>();
            continue;
        }

        // Building must have the item in stock.
        let Ok((mut bld_inv, bld_sprite)) = buildings.get_mut(target_building) else {
            commands.entity(cust_entity).remove::<WantsToBuy>();
            continue;
        };

        if !bld_inv.has(wants.item, 1) {
            event_log.push(LogEvent {
                tick: tick.0,
                agent: cust_name.0.clone(),
                kind: LogKind::Action,
                text: format!("{} is out of {}", bld_sprite.0, wants.item,),
            });
            commands.entity(cust_entity).remove::<WantsToBuy>();
            continue;
        }

        // Execute the transaction.
        bld_inv.remove(wants.item, 1);
        cust_inv.add(wants.item, 1);
        if price > 0 {
            cust_inv.remove(ItemType::GoldCoin, price);
            // Gold goes to the building (could feed into GoldReserve later).
        }

        let item_name = wants.item.to_string();
        let building_name = bld_sprite.0.clone();

        // Log for customer.
        event_log.push(LogEvent {
            tick: tick.0,
            agent: cust_name.0.clone(),
            kind: LogKind::Action,
            text: format!(
                "Bought {} from {} at {} for {}g",
                item_name, worker_name.0, building_name, price,
            ),
        });

        // Log for worker.
        event_log.push(LogEvent {
            tick: tick.0,
            agent: worker_name.0.clone(),
            kind: LogKind::Action,
            text: format!("Sold {} to {} at {}", item_name, cust_name.0, building_name,),
        });

        tracing::info!(
            "Transaction: {} bought {} from {} at {} for {}g",
            cust_name.0,
            item_name,
            worker_name.0,
            building_name,
            price,
        );

        // Transaction complete -- remove the WantsToBuy marker.
        commands.entity(cust_entity).remove::<WantsToBuy>();
    }
}
