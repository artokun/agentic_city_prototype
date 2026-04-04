pub mod bounty;
pub mod jobs;
pub mod map;
pub mod services;
pub mod structures;

use bevy::prelude::*;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(map::MapPlugin)
            .add_plugins(structures::StructurePlugin)
            .add_systems(Update, bounty::advance_board_queue)
            .add_systems(Update, jobs::job_posting_system);
    }
}
