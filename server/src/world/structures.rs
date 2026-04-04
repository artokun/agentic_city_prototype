use bevy::prelude::*;
use uuid::Uuid;

use crate::items::{Inventory, ItemType};
use crate::world::bounty::*;
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
        let mut entity_cmds = commands.spawn((
            StructureId(Uuid::new_v4()),
            GridPos { x: bld.x, y: bld.y },
            SpriteType(bld.name.into()),
            Entrance(entrance),
            Inventory::default(),
            Interactable,
        ));

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
