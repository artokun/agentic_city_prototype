//! Automatically injects bounties to keep the economy flowing.
//! Without injection, agents exhaust all bounties and the economy stalls.
//! This simulates external demand (humans posting bounties via Stripe).

use bevy::prelude::*;
use uuid::Uuid;

use crate::items::ItemType;
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};

/// How often to check and inject bounties (in ticks).
const INJECTION_INTERVAL: u64 = 200; // every 20 seconds

/// Minimum available bounties to maintain on the board.
const MIN_AVAILABLE_BOUNTIES: usize = 3;

/// Pool of bounty templates to randomly inject.
fn bounty_templates() -> Vec<(&'static str, BountyObjective, u32, Vec<(ItemType, u32)>)> {
    vec![
        (
            "Hide a gold egg in a structure",
            BountyObjective::HideItem(ItemType::GoldEgg),
            10,
            vec![(ItemType::GoldEgg, 1)],
        ),
        (
            "Find the hidden gold egg",
            BountyObjective::FindItem(ItemType::GoldEgg),
            10,
            vec![],
        ),
        (
            "Deliver coffee beans to the cafe",
            BountyObjective::RestockDelivery {
                item: ItemType::CoffeeBeans,
                quantity: 10,
                destination: "cafe".into(),
            },
            5,
            vec![],
        ),
        (
            "Deliver flour to the market",
            BountyObjective::RestockDelivery {
                item: ItemType::Flour,
                quantity: 5,
                destination: "market".into(),
            },
            4,
            vec![],
        ),
        (
            "Deliver raw meat to the market",
            BountyObjective::RestockDelivery {
                item: ItemType::RawMeat,
                quantity: 5,
                destination: "market".into(),
            },
            4,
            vec![],
        ),
        (
            "Research project at Google",
            BountyObjective::WorkAtBuilding,
            8,
            vec![],
        ),
        (
            "Inventory audit at the warehouse",
            BountyObjective::WorkAtBuilding,
            6,
            vec![],
        ),
        (
            "Deep clean the hotel",
            BountyObjective::WorkAtBuilding,
            5,
            vec![],
        ),
        // === Social & exploration bounties ===
        (
            "Visit every building in the city and report back",
            BountyObjective::WorkAtBuilding,
            15,
            vec![],
        ),
        (
            "Ask every agent their favorite color and write it down",
            BountyObjective::WorkAtBuilding,
            12,
            vec![],
        ),
        (
            "Hand out coffee coupons to all agents you can find",
            BountyObjective::WorkAtBuilding,
            8,
            vec![],
        ),
        (
            "Write a report on the history of Egyptian cats using Google",
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        ),
        (
            "Interview 2 agents about their daily routine and write a summary",
            BountyObjective::WorkAtBuilding,
            10,
            vec![],
        ),
        (
            "Find the best coffee in town by visiting cafe and market",
            BountyObjective::WorkAtBuilding,
            7,
            vec![],
        ),
        (
            "Deliver a handwritten note to every agent in the city",
            BountyObjective::WorkAtBuilding,
            12,
            vec![],
        ),
        (
            "Talk to 2 agents and collect their business cards",
            BountyObjective::WorkAtBuilding,
            2,
            vec![],
        ),
    ]
}

/// System: inject bounties to maintain minimum supply on the board.
pub fn bounty_injection_system(
    tick: Res<TickCount>,
    mut bounty_registry: ResMut<BountyRegistry>,
) {
    if tick.0 % INJECTION_INTERVAL != 0 {
        return;
    }

    let available_count = bounty_registry.available().len();
    if available_count >= MIN_AVAILABLE_BOUNTIES {
        return;
    }

    let templates = bounty_templates();
    let needed = MIN_AVAILABLE_BOUNTIES - available_count;

    for i in 0..needed {
        // Rotate through templates based on tick to get variety.
        let idx = ((tick.0 / INJECTION_INTERVAL) as usize + i) % templates.len();
        let (desc, objective, reward, claim_items) = &templates[idx];

        // Skip if a similar bounty already exists.
        let already_exists = bounty_registry.bounties.iter().any(|b| {
            b.description == *desc
                && (b.state == BountyState::Available || b.state == BountyState::Claimed)
        });

        if already_exists {
            continue;
        }

        let bounty = Bounty::simple(
            Uuid::new_v4(),
            desc.to_string(),
            objective.clone(),
            *reward,
            claim_items.clone(),
        );

        tracing::info!("[INJECTOR] New bounty: {} ({}g)", desc, reward);
        bounty_registry.bounties.push(bounty);
    }
}
