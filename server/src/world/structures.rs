use bevy::prelude::*;
use uuid::Uuid;

use crate::items::{Inventory, ItemType};
use crate::world::bounty::*;
use crate::world::economy::{GoldReserve, RetailConfig, StockItem, Warehouse};
use crate::world::jobs::{Employer, JobTemplate};
use crate::world::map::{city_buildings, GridPos};

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

/// The entrance tile for a building — agents walk here to enter.
#[derive(Component)]
pub struct Entrance(pub GridPos);

/// Marks a structure as Google — agents can pay gold to search.
#[derive(Component)]
pub struct GoogleBuilding;

/// An agent is currently inside this building.
#[derive(Component)]
pub struct InsideBuilding(pub Entity);

/// Search cost in gold for Google.
pub const GOOGLE_SEARCH_COST: u32 = 1;

fn spawn_structures(mut commands: Commands) {
    let buildings = city_buildings();

    for bld in &buildings {
        let entrance = bld.entrance_pos();
        // Inventory is set per-building in the economy block below.
        let mut entity_cmds = commands.spawn((
            StructureId(Uuid::new_v4()),
            GridPos { x: bld.x, y: bld.y },
            SpriteType(bld.name.into()),
            Entrance(entrance),
            Interactable,
        ));

        // Default empty inventory for buildings without special stock.
        let has_custom_inv = matches!(bld.name, "cafe" | "restaurant" | "diner" | "market" | "warehouse" | "apartments");
        if !has_custom_inv {
            entity_cmds.insert(Inventory::default());
        }

        if bld.name == "google" {
            entity_cmds.insert(GoogleBuilding);
        }

        // Attach employer components to buildings that offer jobs.
        match bld.name {
            "cafe" => {
                entity_cmds.insert(Employer {
                    jobs: vec![JobTemplate {
                        title: "Barista shift".into(),
                        pay_gold: 3,
                        work_duration: 40,
                    }],
                    post_interval: 500,
                    last_posted_tick: 0,
                });
            }
            "restaurant" => {
                entity_cmds.insert(Employer {
                    jobs: vec![JobTemplate {
                        title: "Kitchen help".into(),
                        pay_gold: 4,
                        work_duration: 50,
                    }],
                    post_interval: 600,
                    last_posted_tick: 0,
                });
            }
            "warehouse" => {
                entity_cmds.insert(Employer {
                    jobs: vec![JobTemplate {
                        title: "Stock shelves".into(),
                        pay_gold: 2,
                        work_duration: 30,
                    }],
                    post_interval: 400,
                    last_posted_tick: 0,
                });
            }
            "shop" => {
                entity_cmds.insert(Employer {
                    jobs: vec![JobTemplate {
                        title: "Cashier shift".into(),
                        pay_gold: 3,
                        work_duration: 40,
                    }],
                    post_interval: 500,
                    last_posted_tick: 0,
                });
            }
            "post_office" => {
                entity_cmds.insert(Employer {
                    jobs: vec![JobTemplate {
                        title: "Sort mail".into(),
                        pay_gold: 2,
                        work_duration: 25,
                    }],
                    post_interval: 350,
                    last_posted_tick: 0,
                });
            }
            _ => {}
        }

        // Attach economy components.
        match bld.name {
            "cafe" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 5);
                inv.add(ItemType::Muffin, 5);
                entity_cmds.insert(inv);
                entity_cmds.insert(GoldReserve(5));
                entity_cmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Coffee, max: 20, reorder_at: 2, reorder_qty: 10 },
                        StockItem { item: ItemType::Muffin, max: 20, reorder_at: 2, reorder_qty: 5 },
                    ],
                });
            }
            "restaurant" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Sandwich, 5);
                inv.add(ItemType::Soup, 5);
                entity_cmds.insert(inv);
                entity_cmds.insert(GoldReserve(5));
                entity_cmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Sandwich, max: 20, reorder_at: 2, reorder_qty: 5 },
                        StockItem { item: ItemType::Soup, max: 20, reorder_at: 2, reorder_qty: 8 },
                    ],
                });
            }
            "diner" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 5);
                inv.add(ItemType::Sandwich, 5);
                entity_cmds.insert(inv);
                entity_cmds.insert(GoldReserve(3));
                entity_cmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Coffee, max: 15, reorder_at: 2, reorder_qty: 10 },
                        StockItem { item: ItemType::Sandwich, max: 15, reorder_at: 2, reorder_qty: 5 },
                    ],
                });
            }
            "market" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 5);
                inv.add(ItemType::Muffin, 5);
                inv.add(ItemType::Sandwich, 5);
                inv.add(ItemType::Rations, 5);
                entity_cmds.insert(inv);
                entity_cmds.insert(GoldReserve(8));
                entity_cmds.insert(RetailConfig {
                    stock: vec![
                        StockItem { item: ItemType::Coffee, max: 20, reorder_at: 2, reorder_qty: 10 },
                        StockItem { item: ItemType::Muffin, max: 20, reorder_at: 2, reorder_qty: 5 },
                        StockItem { item: ItemType::Sandwich, max: 20, reorder_at: 2, reorder_qty: 5 },
                        StockItem { item: ItemType::Rations, max: 20, reorder_at: 2, reorder_qty: 10 },
                    ],
                });
            }
            "warehouse" => {
                // Unlimited wholesale supplier.
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 999);
                inv.add(ItemType::Muffin, 999);
                inv.add(ItemType::Rations, 999);
                inv.add(ItemType::Sandwich, 999);
                inv.add(ItemType::Soup, 999);
                entity_cmds.insert(inv);
                entity_cmds.insert(Warehouse);
                entity_cmds.insert(GoldReserve(0));
            }
            "apartments" => {
                // Free rations at home.
                let mut inv = Inventory::default();
                inv.add(ItemType::Rations, 999);
                entity_cmds.insert(inv);
            }
            _ => {}
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
    ));

    // Seed bounties.
    commands.insert_resource(BountyRegistry {
        bounties: vec![
            Bounty {
                id: Uuid::new_v4(),
                description: "Hide a gold egg inside a structure".into(),
                objective: BountyObjective::HideItem(ItemType::GoldEgg),
                reward_gold: 10,
                state: BountyState::Available,
                claimed_by: None,
                claim_items: vec![(ItemType::GoldEgg, 1)],
            },
            Bounty {
                id: Uuid::new_v4(),
                description: "Find the hidden gold egg".into(),
                objective: BountyObjective::FindItem(ItemType::GoldEgg),
                reward_gold: 10,
                state: BountyState::Available,
                claimed_by: None,
                claim_items: vec![],
            },
        ],
    });
}
