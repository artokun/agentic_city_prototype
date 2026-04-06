mod agents;
pub mod config;
mod items;
mod network;
mod process_manager;
mod scenario;
mod tick;
mod world;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksPlugin;
use std::time::Duration;

fn main() {
    tracing_subscriber::fmt::init();

    let registry = process_manager::ProcessRegistry::default();
    process_manager::install_signal_handler(registry.clone());

    App::new()
        .add_plugins(
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(
                config::tick_ms(),
            ))),
        )
        .add_plugins(TokioTasksPlugin::default())
        .insert_resource(process_manager::ProcessRegistryRes(registry))
        .add_plugins(tick::GameTickPlugin)
        .add_plugins(world::WorldPlugin)
        .add_plugins(agents::AgentPlugin)
        .add_plugins(network::NetworkPlugin)
        .run();
}
