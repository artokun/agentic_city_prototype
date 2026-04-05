use bevy::prelude::*;
use uuid::Uuid;

use crate::items::{Inventory, ItemType};
use crate::world::bounty::*;
use crate::world::economy::{GoldReserve, RetailConfig, StockItem, Warehouse};
use crate::world::map::{city_buildings, GridPos};
use crate::world::shifts::Staffable;

pub struct StructurePlugin;

impl Plugin for StructurePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_structures);
    }
}

#[derive(Component)]
pub struct StructureId(pub Uuid);

#[derive(Component)]
pub struct SpriteType(pub String);

#[derive(Component)]
pub struct Interactable;

#[derive(Component)]
pub struct Entrance(pub GridPos);

#[derive(Component)]
pub struct GoogleBuilding;

/// An agent is currently inside this building.
#[derive(Component)]
pub struct InsideBuilding(pub Entity);

fn spawn_structures(mut commands: Commands) {
    let buildings = city_buildings();

    for bld in &buildings {
        let entrance = bld.entrance_pos();
        let mut ecmds = commands.spawn((
            StructureId(Uuid::new_v4()),
            GridPos { x: bld.x, y: bld.y },
            SpriteType(bld.name.into()),
            Entrance(entrance),
            Interactable,
        ));

        match bld.name {
            "cafe" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 5);
                inv.add(ItemType::Muffin, 5);
                ecmds.insert(inv);
                ecmds.insert(Staffable {
                    ticks_per_gold: 1000, // 1g per 150 ticks (~15 sec)
                    worker: None,
                    needs_worker: false,
                    food_perk: true,
                });
                ecmds.insert(GoldReserve(100));
                ecmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Coffee, max: 10, reorder_at: 2, reorder_qty: 10 },
                        StockItem { item: ItemType::Muffin, max: 10, reorder_at: 2, reorder_qty: 5 },
                    ],
                });
            }
            "market" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 5);
                inv.add(ItemType::Muffin, 5);
                inv.add(ItemType::Sandwich, 5);
                inv.add(ItemType::Rations, 5);
                ecmds.insert(inv);
                ecmds.insert(Staffable {
                    ticks_per_gold: 1000,
                    worker: None,
                    needs_worker: false,
                    food_perk: true,
                });
                ecmds.insert(GoldReserve(100));
                ecmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Coffee, max: 10, reorder_at: 2, reorder_qty: 10 },
                        StockItem { item: ItemType::Muffin, max: 10, reorder_at: 2, reorder_qty: 5 },
                        StockItem { item: ItemType::Sandwich, max: 10, reorder_at: 2, reorder_qty: 5 },
                        StockItem { item: ItemType::Rations, max: 10, reorder_at: 2, reorder_qty: 10 },
                    ],
                });
            }
            "warehouse" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::CoffeeBeans, 999);
                inv.add(ItemType::Flour, 999);
                inv.add(ItemType::RawMeat, 999);
                ecmds.insert(inv);
                ecmds.insert(Staffable {
                    ticks_per_gold: 1200,
                    worker: None,
                    needs_worker: false,
                    food_perk: false,
                });
                ecmds.insert(GoldReserve(100));
                ecmds.insert(Warehouse);
            }
            "hotel" => {
                ecmds.insert(Inventory::default());
                ecmds.insert(Staffable {
                    ticks_per_gold: 1100,
                    worker: None,
                    needs_worker: false,
                    food_perk: false,
                });
                ecmds.insert(GoldReserve(100));
            }
            "google" => {
                ecmds.insert(Inventory::default());
                ecmds.insert(GoogleBuilding);
            }
            "apartments" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Rations, 999);
                ecmds.insert(inv);
            }
            "hospital" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Rations, 50);
                ecmds.insert(inv);
                ecmds.insert(GoldReserve(0));
            }
            _ => {
                ecmds.insert(Inventory::default());
            }
        }
    }

    // Bounty board at park edge.
    commands.spawn((
        StructureId(Uuid::new_v4()),
        GridPos { x: 20, y: 32 },
        SpriteType("bounty_board".into()),
        Entrance(GridPos { x: 20, y: 32 }),
        Interactable,
        BountyBoard,
        BoardQueue::default(),
        Inventory::default(),
        BountyTokenStore::default(),
        BountyDropbox::default(),
    ));
}
