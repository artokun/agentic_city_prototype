//! Bounty injection system.
//! Seeds initial bounties from templates on startup, then stops.
//! After the initial set, the System AI generates new bounties dynamically.

use bevy::prelude::*;
use uuid::Uuid;

use crate::items::ItemType;
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};

/// Whether initial bounties have been seeded.
#[derive(Resource)]
pub struct InjectorState {
    pub seeded: bool,
}

impl Default for InjectorState {
    fn default() -> Self {
        Self { seeded: false }
    }
}

struct BountyTemplate {
    title: &'static str,
    instructions: &'static str,
    hidden_criteria: &'static str,
    objective: BountyObjective,
    reward: u32,
    claim_items: Vec<(ItemType, u32)>,
}

/// Initial bounty templates — seeded once at game start.
fn initial_bounties() -> Vec<BountyTemplate> {
    vec![
        BountyTemplate {
            title: "Research project at Google",
            instructions: "Go to Google and use search_internet (costs 1g) to do research. This produces a document in your inventory. Return to the board, use deposit_item to deposit each document at the bounty board, then use complete_bounty.",
            hidden_criteria: "GM: STRICT — verify 'document' items are deposited at the BOUNTY BOARD's inventory (not the agent's). If docs are in agent inventory but not the board, REJECT and tell them to deposit_item first. If no document, REJECT. Simply visiting Google is NOT enough.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 8,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Hide the golden egg",
            instructions: "You receive a gold_egg item on claim. Go to any building and use deposit_item to transfer it from your inventory into the building. Return to the board, then use complete_bounty to submit.",
            hidden_criteria: "GM: verify gold_egg exists in ANY structure's inventory (not in the agent's inventory).",
            objective: BountyObjective::HideItem(ItemType::GoldEgg),
            reward: 2,
            claim_items: vec![(ItemType::GoldEgg, 1)],
        },
        BountyTemplate {
            title: "Warehouse inventory audit",
            instructions: "Go to the warehouse and use look_around to inspect the inventory. Then use create_document (service='warehouse_audit.md', text='<your findings in markdown>') to write up what you found. Return to the board, use deposit_item to deposit each document at the bounty board, then use complete_bounty.",
            hidden_criteria: "GM: STRICT — verify 'document' items are deposited at the BOUNTY BOARD's inventory. If docs are in agent inventory but not the board, REJECT and tell them to deposit_item first AND visited the warehouse. If no document, REJECT.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 6,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Deep clean the hotel",
            instructions: "Go to the hotel and complete a cleaning job. Return to the board and use complete_bounty to submit.",
            hidden_criteria: "GM: verify agent visited the hotel.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 5,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "City tour: visit every building",
            instructions: "Visit all buildings in the city (bounty_board, cafe, market, warehouse, hotel, apartments, google, hospital). Return to the board, deposit any proof items, then use complete_bounty.",
            hidden_criteria: "GM: verify agent visited at least 6 different buildings. Check action log for building arrivals.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 15,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Exchange business cards",
            instructions: "Start a conversation with another agent. Cards are exchanged automatically when you chat face-to-face.",
            hidden_criteria: "GM: verify agent has at least 1 contact in their business_cards.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 3,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Egyptian cats research paper",
            instructions: "Go to Google and use search_internet (costs 1g) to research the history of Egyptian cats. This produces a document. Return to the board, use deposit_item to deposit each document at the bounty board, then use complete_bounty.",
            hidden_criteria: "GM: STRICT — verify 'document' items are deposited at the BOUNTY BOARD's inventory. If docs are in agent inventory but not the board, REJECT and tell them to deposit_item first. Simply visiting Google is NOT enough.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 10,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Agent interviews",
            instructions: "Interview 2 agents about their daily routine. Start a conversation with each, ask questions, then use create_document (service='interview_<name>.md', text='<markdown notes>') to write up each interview. Go to the bounty board and use deposit_item for each document, then use complete_bounty.",
            hidden_criteria: "GM: STRICT — verify at least 2 document items are deposited at the BOUNTY BOARD (not in the agent's inventory). If documents are in the agent's inventory but not the board, REJECT and tell them to deposit_item first. Also verify at least 2 conversations in the action log.",
            objective: BountyObjective::WorkAtBuilding,
            reward: 10,
            claim_items: vec![],
        },
        BountyTemplate {
            title: "Find the hidden golden egg",
            instructions: "Another agent hid a gold_egg in a structure. Visit buildings and use take_item to pick it up from the building's inventory.",
            hidden_criteria: "GM: verify agent has gold_egg in their inventory.",
            objective: BountyObjective::FindItem(ItemType::GoldEgg),
            reward: 5,
            claim_items: vec![],
        },
    ]
}

/// System: seed initial bounties once, then stop.
/// If a `ScenarioBountyConfig` resource exists, this system is a no-op
/// (scenario bounties are seeded by `scenario::seed_scenario_bounties_system`).
pub fn bounty_injection_system(
    tick: Res<TickCount>,
    mut bounty_registry: ResMut<BountyRegistry>,
    mut state: ResMut<InjectorState>,
    scenario_config: Option<Res<crate::scenario::ScenarioBountyConfig>>,
) {
    if state.seeded { return; }
    // If scenario bounties are configured, skip normal injection entirely.
    if scenario_config.is_some() { state.seeded = true; return; }
    // Wait for tick 0 to seed.
    if tick.0 > 10 { state.seeded = true; return; }
    if tick.0 != 0 { return; }

    let templates = initial_bounties();
    for tmpl in &templates {
        let mut bounty = Bounty::simple(
            Uuid::new_v4(),
            tmpl.title.to_string(),
            tmpl.objective.clone(),
            tmpl.reward,
            tmpl.claim_items.clone(),
        );
        bounty.hidden_criteria = format!(
            "Instructions for agent: {}\n\nGM: {}",
            tmpl.instructions, tmpl.hidden_criteria
        );
        bounty_registry.bounties.push(bounty);
    }

    tracing::info!("[INJECTOR] Seeded {} initial bounties", templates.len());
    state.seeded = true;
}
