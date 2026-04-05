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

        app.init_resource::<BroadcastTx>()
            .init_resource::<action_handler::PendingActions>()
            .init_resource::<action_handler::SuggestionBox>()
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
            .add_systems(Update, action_handler::process_deposits_system.after(action_handler::apply_mcp_actions_system));
    }
}

#[derive(Resource)]
struct CommandSenderHolder(mpsc::Sender<commands::GameCommand>);

#[derive(Resource, Clone)]
struct RelaysHolder(AgentRelays);

fn spawn_axum(
    runtime: ResMut<TokioTasksRuntime>,
    broadcast_tx: Res<BroadcastTx>,
    cmd_sender: Res<CommandSenderHolder>,
    relays: Res<RelaysHolder>,
) {
    let state = AppState {
        broadcast_tx: broadcast_tx.sender.clone(),
        command_tx: cmd_sender.0.clone(),
        stripe_secret: std::env::var("STRIPE_SECRET_KEY").ok(),
        agent_relays: relays.0.clone(),
    };

    runtime.spawn_background_task(|_ctx| async move {
        ws::start_server(state).await;
    });
}
