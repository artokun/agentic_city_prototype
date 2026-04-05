pub mod action_handler;
pub mod agent_relay;
pub mod broadcast;
pub mod commands;
pub mod serializer;
pub mod ws;

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use tokio::sync::mpsc;

use self::agent_relay::AgentRelays;
use self::broadcast::BroadcastTx;
use self::commands::CommandReceiver;
use self::ws::AppState;
use crate::agents::ai::AgentRelaysResource;

pub struct NetworkPlugin;

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let relays = AgentRelays::default();
        let world_json = std::sync::Arc::new(std::sync::RwLock::new("{}".to_string()));

        app.init_resource::<BroadcastTx>()
            .init_resource::<action_handler::PendingActions>()
            .init_resource::<action_handler::SuggestionBox>()
            .init_resource::<commands::PendingVerdicts>()
            .init_resource::<commands::PendingDocuments>()
            .insert_resource(broadcast::WorldStateJsonHolder(world_json.clone()))
            .insert_resource(WorldJsonArc(world_json))
            .insert_resource(CommandReceiver { rx: cmd_rx })
            .insert_resource(CommandSenderHolder(cmd_tx))
            .insert_resource(AgentRelaysResource(relays.clone()))
            .insert_resource(RelaysHolder(relays))
            .add_systems(Startup, spawn_axum)
            .add_systems(Update, broadcast::broadcast_state)
            .add_systems(Update, commands::process_commands_system)
            .add_systems(Update, action_handler::apply_mcp_actions_system)
            .add_systems(Update, action_handler::apply_conversation_messages_system.after(action_handler::apply_mcp_actions_system))
            .add_systems(Update, action_handler::auto_exchange_cards_system.after(action_handler::apply_mcp_actions_system))
            .add_systems(Update, action_handler::process_deposits_system.after(action_handler::apply_mcp_actions_system))
            .add_systems(Update, action_handler::give_claim_items_system.after(action_handler::apply_mcp_actions_system))
            .add_systems(Update, action_handler::process_gm_verdicts_system.after(commands::process_commands_system))
            .add_systems(Update, action_handler::deliver_documents_system.after(commands::process_commands_system))
            .add_systems(Update, broadcast::update_world_state_json);
    }
}

#[derive(Resource)]
struct CommandSenderHolder(mpsc::Sender<commands::GameCommand>);

#[derive(Resource, Clone)]
struct RelaysHolder(AgentRelays);

#[derive(Resource, Clone)]
struct WorldJsonArc(std::sync::Arc<std::sync::RwLock<String>>);

fn spawn_axum(
    runtime: ResMut<TokioTasksRuntime>,
    broadcast_tx: Res<BroadcastTx>,
    cmd_sender: Res<CommandSenderHolder>,
    relays: Res<RelaysHolder>,
    world_json: Res<WorldJsonArc>,
) {
    let state = AppState {
        broadcast_tx: broadcast_tx.sender.clone(),
        command_tx: cmd_sender.0.clone(),
        stripe_secret: std::env::var("STRIPE_SECRET_KEY").ok(),
        agent_relays: relays.0.clone(),
        world_state_json: world_json.0.clone(),
        documents: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
    };

    runtime.spawn_background_task(|_ctx| async move {
        ws::start_server(state).await;
    });
}
