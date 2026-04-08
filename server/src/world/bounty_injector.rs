//! Bounty injection system.
//! Seeds initial bounties from a JSON file on startup, then stops.
//! After the initial set, the System AI generates new bounties dynamically.

use bevy::prelude::*;
use serde::Deserialize;
use uuid::Uuid;

use crate::items::ItemType;
use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyBoard, BountyObjective, BountyTokenStore};

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

#[derive(Deserialize)]
struct JsonBounty {
    title: String,
    instructions: String,
    hidden_criteria: String,
    objective: String,
    reward: u32,
    #[serde(default)]
    claim_items: Vec<(String, u32)>,
}

fn parse_item_type(s: &str) -> Option<ItemType> {
    match s {
        "gold_coin" => Some(ItemType::GoldCoin),
        "gold_egg" => Some(ItemType::GoldEgg),
        "coffee" => Some(ItemType::Coffee),
        "muffin" => Some(ItemType::Muffin),
        "rations" => Some(ItemType::Rations),
        "sandwich" => Some(ItemType::Sandwich),
        "soup" => Some(ItemType::Soup),
        "document" => Some(ItemType::Document),
        "coffee_beans" => Some(ItemType::CoffeeBeans),
        "flour" => Some(ItemType::Flour),
        "raw_meat" => Some(ItemType::RawMeat),
        "bounty_token" => Some(ItemType::BountyToken),
        "paycheck" => Some(ItemType::Paycheck),
        _ => None,
    }
}

fn parse_objective(s: &str) -> BountyObjective {
    if let Some(item_name) = s.strip_prefix("HideItem(").and_then(|s| s.strip_suffix(')')) {
        if let Some(item) = parse_item_type(item_name) {
            return BountyObjective::HideItem(item);
        }
    }
    if let Some(item_name) = s.strip_prefix("FindItem(").and_then(|s| s.strip_suffix(')')) {
        if let Some(item) = parse_item_type(item_name) {
            return BountyObjective::FindItem(item);
        }
    }
    BountyObjective::WorkAtBuilding
}

/// Load bounties from bounties.json at runtime.
fn load_bounties_from_json() -> Vec<JsonBounty> {
    let path = std::env::var("BOUNTIES_FILE").unwrap_or_else(|_| "bounties.json".to_string());
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<Vec<JsonBounty>>(&contents) {
            Ok(bounties) => {
                tracing::info!("[INJECTOR] Loaded {} bounties from {}", bounties.len(), path);
                bounties
            }
            Err(e) => {
                tracing::error!("[INJECTOR] Failed to parse {}: {}", path, e);
                vec![]
            }
        },
        Err(e) => {
            tracing::warn!("[INJECTOR] Could not read {}: {} — no initial bounties", path, e);
            vec![]
        }
    }
}

/// System: seed initial bounties once, then stop.
/// If a `ScenarioBountyConfig` resource exists, this system is a no-op
/// (scenario bounties are seeded by `scenario::seed_scenario_bounties_system`).
pub fn bounty_injection_system(
    tick: Res<TickCount>,
    mut boards: Query<&mut BountyTokenStore, With<BountyBoard>>,
    mut state: ResMut<InjectorState>,
    scenario_config: Option<Res<crate::scenario::ScenarioBountyConfig>>,
) {
    if state.seeded {
        return;
    }
    // If scenario bounties are configured, skip normal injection entirely.
    if scenario_config.is_some() {
        state.seeded = true;
        return;
    }
    // Wait for tick 0 to seed.
    if tick.0 > 10 {
        state.seeded = true;
        return;
    }
    if tick.0 != 0 {
        return;
    }

    let Some(mut store) = boards.iter_mut().next() else {
        return;
    };

    let templates = load_bounties_from_json();
    for tmpl in &templates {
        let objective = parse_objective(&tmpl.objective);
        let claim_items: Vec<(ItemType, u32)> = tmpl
            .claim_items
            .iter()
            .filter_map(|(name, count)| parse_item_type(name).map(|item| (item, *count)))
            .collect();
        let mut bounty = Bounty::simple(
            Uuid::new_v4(),
            tmpl.title.clone(),
            objective,
            tmpl.reward,
            claim_items,
        );
        bounty.hidden_criteria = format!(
            "Instructions for agent: {}\n\nGM: {}",
            tmpl.instructions, tmpl.hidden_criteria
        );
        store.tokens.insert(bounty.id, bounty);
    }

    tracing::info!("[INJECTOR] Seeded {} initial bounties", templates.len());
    state.seeded = true;
}
