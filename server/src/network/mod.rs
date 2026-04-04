pub mod broadcast;
pub mod commands;
pub mod serializer;
pub mod ws;

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use tokio::sync::mpsc;

use self::broadcast::BroadcastTx;
use self::commands::CommandReceiver;
use self::ws::AppState;

pub struct NetworkPlugin;

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);

        app.init_resource::<BroadcastTx>()
            .insert_resource(CommandReceiver { rx: cmd_rx })
            .insert_resource(CommandSenderHolder(cmd_tx))
            .add_systems(Startup, spawn_axum)
            .add_systems(Update, broadcast::broadcast_state)
            .add_systems(Update, commands::process_commands_system);
    }
}

#[derive(Resource)]
struct CommandSenderHolder(mpsc::Sender<commands::GameCommand>);

fn spawn_axum(
    runtime: ResMut<TokioTasksRuntime>,
    broadcast_tx: Res<BroadcastTx>,
    cmd_sender: Res<CommandSenderHolder>,
) {
    let state = AppState {
        broadcast_tx: broadcast_tx.sender.clone(),
        command_tx: cmd_sender.0.clone(),
        stripe_secret: std::env::var("STRIPE_SECRET_KEY").ok(),
    };

    runtime.spawn_background_task(|_ctx| async move {
        ws::start_server(state).await;
    });
}
