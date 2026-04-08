mod agents;
pub mod config;
mod items;
mod llm;
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
    // Load .env file for API keys (OPENAI_API_KEY, etc.).
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("[ENV] No .env file loaded: {e}");
    }

    tracing_subscriber::fmt::init();

    // Load LLM config from TOML.
    let llm_config = match llm::config::LlmConfig::from_file("config/llm.toml") {
        Ok(cfg) => {
            tracing::info!(
                "[LLM] Loaded config: {} providers, {} profiles",
                cfg.providers.len(),
                cfg.profiles.len()
            );
            for (name, p) in &cfg.providers {
                tracing::info!("[LLM]   provider '{}': type={}, model={}", name, p.provider_type, p.model);
            }
            for (name, p) in &cfg.profiles {
                let model = p.model.as_deref().unwrap_or("(provider default)");
                tracing::info!("[LLM]   profile '{}': provider={}, model={}, compact={}", name, p.provider, model, p.compact_threshold);
            }
            cfg
        }
        Err(e) => {
            tracing::error!("[LLM] Could not load config/llm.toml: {e}");
            tracing::error!("[LLM] Agent sessions require valid provider/profile config. Create config/llm.toml or copy from the repo.");
            std::process::exit(1);
        }
    };

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
        .insert_resource(llm_config)
        .init_resource::<llm::session_registry::SessionRegistry>()
        .insert_resource(llm::supervisor::SessionSupervisor::new())
        .add_plugins(tick::GameTickPlugin)
        .add_plugins(world::WorldPlugin)
        .add_plugins(agents::AgentPlugin)
        .add_plugins(network::NetworkPlugin)
        .add_plugins(llm::LlmPlugin)
        .run();
}
