pub mod bounty;
pub mod bounty_contract;
pub mod economy;
pub mod jobs;
pub mod map;
pub mod services;
pub mod shifts;
pub mod structures;
pub mod transactions;

use bevy::prelude::*;

use bounty_contract::ContractBoard;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(map::MapPlugin)
            .add_plugins(structures::StructurePlugin)
            .init_resource::<ContractBoard>()
            .add_systems(Update, bounty::advance_board_queue)
            .add_systems(Update, jobs::job_posting_system)
            .add_systems(Update, bounty_contract::bounty_expiry_system)
            .add_systems(Update, economy::auto_restock_system)
            .add_systems(Update, shifts::shift_demand_system)
            .add_systems(Update, shifts::shift_tracking_system)
            .add_systems(Update, transactions::transaction_system);
    }
}
