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
            action_name: "eat_cafe",
            gold_cost: 1,
            duration_ticks: 10,
            effects: ServiceEffects { hunger: 40.0, ..Default::default() },
            consumes_item: Some(ItemType::Muffin),
        },
        BuildingService {
            building_name: "restaurant",
            action_name: "eat_restaurant",
            gold_cost: 2,
            duration_ticks: 15,
            effects: ServiceEffects { hunger: 60.0, boredom: 10.0, ..Default::default() },
            consumes_item: Some(ItemType::Sandwich),
        },
        BuildingService {
            building_name: "apartments",
            action_name: "cook_at_home",
            gold_cost: 0,
            duration_ticks: 30,
            effects: ServiceEffects { hunger: 60.0, ..Default::default() },
            consumes_item: Some(ItemType::Rations),
        },
        BuildingService {
            building_name: "diner",
            action_name: "eat_diner",
            gold_cost: 1,
            duration_ticks: 12,
            effects: ServiceEffects { hunger: 45.0, ..Default::default() },
            consumes_item: Some(ItemType::Soup),
        },
        // --- Sleep ---
        BuildingService {
            building_name: "hotel",
            action_name: "sleep_hotel",
            gold_cost: 1,
            duration_ticks: 30,
            effects: ServiceEffects { energy: 50.0, ..Default::default() },
            consumes_item: None,
        },
        BuildingService {
            building_name: "apartments",
            action_name: "sleep_at_home",
            gold_cost: 0,
            duration_ticks: 50,
            effects: ServiceEffects { energy: 80.0, ..Default::default() },
            consumes_item: None,
        },
        // --- Energy boost ---
        BuildingService {
            building_name: "cafe",
            action_name: "buy_coffee",
            gold_cost: 1,
            duration_ticks: 5,
            effects: ServiceEffects { energy: 20.0, ..Default::default() },
            consumes_item: Some(ItemType::Coffee),
        },
        // --- Boredom ---
        BuildingService {
            building_name: "library",
            action_name: "read_library",
            gold_cost: 0,
            duration_ticks: 20,
            effects: ServiceEffects { boredom: 30.0, ..Default::default() },
            consumes_item: None,
        },
        BuildingService {
            building_name: "theater",
            action_name: "watch_show",
            gold_cost: 2,
            duration_ticks: 25,
            effects: ServiceEffects { boredom: 50.0, ..Default::default() },
            consumes_item: None,
        },
        BuildingService {
            building_name: "gym",
            action_name: "work_out",
            gold_cost: 0,
            duration_ticks: 20,
            effects: ServiceEffects { boredom: 25.0, energy: -10.0, hunger: -10.0, ..Default::default() },
            consumes_item: None,
        },
        // --- Information ---
        BuildingService {
            building_name: "google",
            action_name: "search_internet",
            gold_cost: 1,
            duration_ticks: 10,
            effects: ServiceEffects::default(),
            consumes_item: None,
        },
        // --- Bounty board ---
        BuildingService {
            building_name: "bounty_board",
            action_name: "check_board",
            gold_cost: 0,
            duration_ticks: 5,
            effects: ServiceEffects::default(),
            consumes_item: None,
        },
    ]
}

/// Get services available at a building.
pub fn services_at(building_name: &str) -> Vec<BuildingService> {
    all_services()
        .into_iter()
        .filter(|s| s.building_name == building_name)
        .collect()
}
