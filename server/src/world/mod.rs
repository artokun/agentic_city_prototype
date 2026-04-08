pub mod bounty;
pub mod bounty_injector;
pub mod economy;
pub mod hospital;
pub mod jobs;
pub mod map;
pub mod services;
pub mod shifts;
pub mod structures;
pub mod transactions;

use bevy::prelude::*;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(map::MapPlugin)
            .add_plugins(structures::StructurePlugin)
            .add_systems(Update, bounty::advance_board_queue)
            .add_systems(Update, bounty::bounty_expiry_system)
            .add_systems(Update, jobs::job_posting_system)
            .add_systems(Update, economy::auto_restock_system)
            .add_systems(Update, shifts::shift_processing_system)
            .add_systems(Update, shifts::shift_demand_system)
            .add_systems(Update, shifts::shift_tracking_system)
            .add_systems(Update, transactions::transaction_system)
            .init_resource::<bounty::Library>()
            .init_resource::<bounty_injector::InjectorState>()
            .add_systems(Update, bounty_injector::bounty_injection_system)
            .add_systems(Update, hospital::pass_out_system)
            .add_systems(Update, hospital::hospital_recovery_system);
    }
}
