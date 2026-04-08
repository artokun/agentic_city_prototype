/// Defines what each building offers agents.
/// This is the data agents use to make economic decisions.
use crate::items::ItemType;

#[derive(Debug, Clone)]
pub struct BuildingService {
    pub building_name: &'static str,
    pub action_name: &'static str,
    pub gold_cost: u32,
    pub duration_ticks: u32,
    pub effects: ServiceEffects,
    /// Item consumed from the building's inventory per use.
    pub consumes_item: Option<ItemType>,
    /// Item produced and given to the agent after using this service.
    pub produces_item: Option<ItemType>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceEffects {
    pub energy: f32,
    pub hunger: f32,
    pub boredom: f32,
}

/// All available services in the city.
pub fn all_services() -> Vec<BuildingService> {
    vec![
        // --- Food ---
        BuildingService {
            building_name: "cafe",
            action_name: "buy_muffin",
            gold_cost: 1,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: Some(ItemType::Muffin),
            produces_item: Some(ItemType::Muffin),
        },
        BuildingService {
            building_name: "market",
            action_name: "buy_sandwich",
            gold_cost: 2,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: Some(ItemType::Sandwich),
            produces_item: Some(ItemType::Sandwich),
        },
        BuildingService {
            building_name: "market",
            action_name: "buy_rations",
            gold_cost: 1,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: Some(ItemType::Rations),
            produces_item: Some(ItemType::Rations),
        },
        // --- Sleep ---
        BuildingService {
            building_name: "hotel",
            action_name: "sleep_hotel",
            gold_cost: 1,
            duration_ticks: 30,
            effects: ServiceEffects {
                energy: 50.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        BuildingService {
            building_name: "apartments",
            action_name: "sleep_at_home",
            gold_cost: 0,
            duration_ticks: 50,
            effects: ServiceEffects {
                energy: 80.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        // --- Energy boost ---
        BuildingService {
            building_name: "cafe",
            action_name: "buy_coffee",
            gold_cost: 1,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: Some(ItemType::Coffee),
            produces_item: Some(ItemType::Coffee),
        },
        // --- Boredom ---
        BuildingService {
            building_name: "library",
            action_name: "read_library",
            gold_cost: 0,
            duration_ticks: 20,
            effects: ServiceEffects {
                boredom: 30.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        // --- Boredom recovery ---
        BuildingService {
            building_name: "cafe",
            action_name: "hang_out",
            gold_cost: 0,
            duration_ticks: 15,
            effects: ServiceEffects {
                boredom: 25.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        BuildingService {
            building_name: "hotel",
            action_name: "relax_in_lobby",
            gold_cost: 0,
            duration_ticks: 10,
            effects: ServiceEffects {
                boredom: 15.0,
                energy: 5.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        BuildingService {
            building_name: "market",
            action_name: "window_shop",
            gold_cost: 0,
            duration_ticks: 10,
            effects: ServiceEffects {
                boredom: 15.0,
                ..Default::default()
            },
            consumes_item: None,
            produces_item: None,
        },
        // --- Information ---
        BuildingService {
            building_name: "google",
            action_name: "search_internet",
            gold_cost: 1,
            duration_ticks: 10,
            effects: ServiceEffects::default(),
            consumes_item: None,
            produces_item: Some(ItemType::Document),
        },
        // --- Bounty board ---
        BuildingService {
            building_name: "bounty_board",
            action_name: "redeem_paycheck",
            gold_cost: 0,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: None,
            produces_item: None,
        },
    ]
}

/// Get services available at a building.
#[allow(dead_code)]
pub fn services_at(building_name: &str) -> Vec<BuildingService> {
    all_services()
        .into_iter()
        .filter(|s| s.building_name == building_name)
        .collect()
}
