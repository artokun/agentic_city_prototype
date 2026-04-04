use bevy::prelude::*;
use uuid::Uuid;

use crate::items::{Inventory, ItemType};
use crate::world::bounty::*;
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
                    ticks_per_gold: 20, // 1g per 20 ticks (~2 sec)
                    worker: None,
                    needs_worker: false,
                    food_perk: true,
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
                    ticks_per_gold: 20,
                    worker: None,
                    needs_worker: false,
                    food_perk: true,
                });
            }
            "warehouse" => {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 999);
                inv.add(ItemType::Muffin, 999);
                inv.add(ItemType::Rations, 999);
                inv.add(ItemType::Sandwich, 999);
                inv.add(ItemType::Soup, 999);
                ecmds.insert(inv);
                ecmds.insert(Staffable {
                    ticks_per_gold: 30,
                    worker: None,
                    needs_worker: false,
                    food_perk: false,
                });
            }
            "hotel" => {
                ecmds.insert(Inventory::default());
                ecmds.insert(Staffable {
                    ticks_per_gold: 25,
                    worker: None,
                    needs_worker: false,
                    food_perk: false,
                });
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
