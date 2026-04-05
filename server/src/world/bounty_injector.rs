//! Automatically injects bounties to keep the economy flowing.
//! Without injection, agents exhaust all bounties and the economy stalls.
//! This simulates external demand (humans posting bounties via Stripe).

use bevy::prelude::*;
use uuid::Uuid;

use crate::items::ItemType;
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};

/// How often to check and inject bounties (in ticks).
const INJECTION_INTERVAL: u64 = 200;

/// Minimum available bounties to maintain on the board.
const MIN_AVAILABLE_BOUNTIES: usize = 3;

struct BountyTemplate {
    title: &'static str,
    instructions: &'static str,
    hidden_criteria: &'static str,
    objective: BountyObjective,
    reward: u32,
    claim_items: Vec<(ItemType, u32)>,
}

/// Pool of bounty templates to randomly inject.
fn bounty_templates() -> Vec<BountyTemplate> {
    vec![
        // --- Free bounties first (no gold required) ---
        BountyTemplate {
            title: "Research project at Google",
            instructions: "Go to Google and use search_internet (costs 1g) to do research. This produces a document in your inventory. Return to the board with the document.",
            hidden_criteria: "GM: STRICT — verify agent has a 'document' item in their inventory. If no document, REJECT. Simply visiting Google is NOT enough — they must use search_internet which costs 1g and produces a document.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 8,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Hide the golden egg",
            instructions: "You receive a gold_egg item on claim. Go to any building and use deposit_item with service='gold_egg' to hide it. Return to the board to collect reward.",
            hidden_criteria: "GM: verify gold_egg exists in ANY structure's inventory (not in the agent's inventory).",
            objective: BountyObjective::HideItem(ItemType::GoldEgg),
            reward: 2,
            claim_items: vec![(ItemType::GoldEgg, 1)],
        },
        BountyTemplate {
            title: "Warehouse inventory audit",
            instructions: "Go to the warehouse and use look_around to inspect the inventory. Report back to the board.",
            hidden_criteria: "GM: verify agent visited the warehouse AND used look_around there (check action log for both warehouse arrival AND look_around action while at warehouse position).",
            objective: BountyObjective::WorkAtBuilding,
            reward: 6,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deep clean the hotel",
            instructions: "Go to the hotel and complete a cleaning job. Return to the board.",
            hidden_criteria: "GM: verify agent visited the hotel.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 5,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "City tour: visit every building",
            instructions: "Visit all buildings in the city (bounty_board, cafe, market, warehouse, hotel, apartments, google, hospital). Return to the board when done.",
            hidden_criteria: "GM: verify agent visited at least 6 different buildings. Check action log for building arrivals.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 15,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Exchange business cards",
            instructions: "Start a conversation with another agent. Cards are exchanged automatically when you chat face-to-face.",
            hidden_criteria: "GM: verify agent has at least 1 contact in their business_cards. Check if cards_remaining < 5.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 3,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Egyptian cats research paper",
            instructions: "Go to Google and use search_internet (costs 1g) to research the history of Egyptian cats. This produces a document. Return to the board with the document.",
            hidden_criteria: "GM: STRICT — verify agent has a 'document' item in inventory. Simply visiting Google is NOT enough. They must have used search_internet which produces a document.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 10,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Agent interviews",
            instructions: "Interview 2 agents about their daily routine. Start conversations and ask them questions.",
            hidden_criteria: "GM: verify agent started at least 2 conversations (check action log for start_conversation events).",
            objective: BountyObjective::WorkAtBuilding,
            reward: 10,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Best coffee in town",
            instructions: "Visit the cafe and market to find the best coffee. Compare what's available.",
            hidden_criteria: "GM: verify agent visited both cafe and market.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 7,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Color survey",
            instructions: "Ask every agent their favorite color. Start conversations and collect answers.",
            hidden_criteria: "GM: verify agent started at least 2 conversations.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 12,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deliver notes to all agents",
            instructions: "Find and deliver a handwritten note to every agent in the city. Use send_message or start_conversation.",
            hidden_criteria: "GM: verify agent sent messages to at least 2 other agents.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 12,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Coffee coupon distribution",
            instructions: "Hand out coffee coupons to all agents. Find them and start conversations.",
            hidden_criteria: "GM: verify agent started at least 1 conversation.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 8,
            claim_items: vec![],
        },
        // --- Bounties that may require gold or searching ---
        BountyTemplate {
            title: "Find the hidden golden egg",
            instructions: "Another agent hid a gold_egg in a structure. Visit buildings and check their inventory to find it.",
            hidden_criteria: "GM: verify agent has gold_egg in their inventory, OR verify gold_egg was removed from a structure's inventory by this agent.",
            objective: BountyObjective::FindItem(ItemType::GoldEgg),
            reward: 5,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deliver coffee beans to cafe",
            instructions: "Buy coffee beans from the warehouse and deliver them to the cafe.",
            hidden_criteria: "GM: verify cafe received coffee_beans delivery (check cafe inventory for coffee_beans increase).",
            objective: BountyObjective::RestockDelivery {
                item: ItemType::CoffeeBeans,
                quantity: 10,
                destination: "cafe".into(),
            },
            reward: 5,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deliver flour to market",
            instructions: "Buy flour from the warehouse and deliver it to the market.",
            hidden_criteria: "GM: verify market received flour delivery.",
            objective: BountyObjective::RestockDelivery {
                item: ItemType::Flour,
                quantity: 5,
                destination: "market".into(),
            },
            reward: 4,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deliver raw meat to market",
            instructions: "Buy raw meat from the warehouse and deliver it to the market.",
            hidden_criteria: "GM: verify market received raw_meat delivery.",
            objective: BountyObjective::RestockDelivery {
                item: ItemType::RawMeat,
                quantity: 5,
                destination: "market".into(),
            },
            reward: 4,
            claim_items: vec![],
        },
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
        let idx = ((tick.0 / INJECTION_INTERVAL) as usize + i) % templates.len();
        let tmpl = &templates[idx];

        // Skip if a similar bounty already exists.
        let already_exists = bounty_registry.bounties.iter().any(|b| {
            b.description == tmpl.title
                && (b.state == BountyState::Available || b.state == BountyState::Claimed)
        });

        if already_exists {
            continue;
        }

        let mut bounty = Bounty::simple(
            Uuid::new_v4(),
            tmpl.title.to_string(),
            tmpl.objective.clone(),
            tmpl.reward,
            tmpl.claim_items.clone(),
        );
        bounty.hidden_criteria = format!(
            "Instructions for agent: {}\n\n{}",
            tmpl.instructions, tmpl.hidden_criteria
        );

        tracing::info!("[INJECTOR] New bounty: {} ({}g)", tmpl.title, tmpl.reward);
        bounty_registry.bounties.push(bounty);
    }
}
