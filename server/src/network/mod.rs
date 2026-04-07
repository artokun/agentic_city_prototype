pub mod action_handler;
pub mod agent_relay;
pub mod broadcast;
pub mod commands;
pub mod serializer;
pub mod system_relay;
pub mod ws;

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use tokio::sync::mpsc;

use self::agent_relay::AgentRelays;
use self::broadcast::BroadcastTx;
use self::commands::CommandReceiver;
use self::system_relay::{SystemRelay, SystemRelayResource};
use self::ws::AppState;
use crate::agents::ai::AgentRelaysResource;

pub struct NetworkPlugin;

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let relays = AgentRelays::default();
        let system_relay = SystemRelay::default();
        let world_json = std::sync::Arc::new(std::sync::RwLock::new("{}".to_string()));
        let library_json = std::sync::Arc::new(std::sync::RwLock::new("[]".to_string()));
        let debug_log: std::sync::Arc<std::sync::RwLock<Vec<ws::DebugEntry>>> =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));

        // Create logs directory and initialize run log file.
        let log_dir = std::path::PathBuf::from("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let log_path = log_dir.join(format!("run_{}.jsonl", timestamp));
        // Create/truncate the file.
        let _ = std::fs::File::create(&log_path);
        // Update the `latest` symlink.
        let latest = log_dir.join("latest.jsonl");
        let _ = std::fs::remove_file(&latest);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&log_path.file_name().unwrap(), &latest);
        tracing::info!("[LOGS] Writing to {}", log_path.display());

        app.init_resource::<BroadcastTx>()
            .init_resource::<action_handler::PendingActions>()
            .init_resource::<action_handler::SuggestionBox>()
            .init_resource::<commands::PendingVerdicts>()
            .init_resource::<commands::PendingDocuments>()
            .init_resource::<commands::PendingGoldGrants>()
            .insert_resource(broadcast::WorldStateJsonHolder(world_json.clone()))
            .insert_resource(WorldJsonArc(world_json))
            .insert_resource(LibraryJsonArc(library_json))
            .insert_resource(DebugLogArc(debug_log.clone()))
            .init_resource::<DebugLogCursor>()
            .insert_resource(RunLogFile(log_path))
            .insert_resource(CommandReceiver { rx: cmd_rx })
            .insert_resource(CommandSenderHolder(cmd_tx))
            .insert_resource(AgentRelaysResource(relays.clone()))
            .insert_resource(SystemRelayResource(system_relay.clone()))
            .insert_resource(RelaysHolder(relays))
            .insert_resource(SystemRelayHolder(system_relay))
            .add_systems(Startup, spawn_axum)
            .add_systems(Update, broadcast::broadcast_state)
            .add_systems(Update, commands::process_commands_system)
            .add_systems(Update, action_handler::apply_mcp_actions_system)
            .add_systems(
                Update,
                action_handler::apply_conversation_messages_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::auto_exchange_cards_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::process_deposits_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::cleanup_abandoned_dropbox_slots_system
                    .after(action_handler::process_deposits_system),
            )
            .add_systems(
                Update,
                action_handler::process_take_items_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::process_create_documents_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::give_claim_items_system
                    .after(action_handler::apply_mcp_actions_system),
            )
            .add_systems(
                Update,
                action_handler::process_gm_verdicts_system.after(commands::process_commands_system),
            )
            .add_systems(
                Update,
                action_handler::deliver_documents_system.after(commands::process_commands_system),
            )
            .add_systems(
                Update,
                action_handler::process_gold_grants_system.after(commands::process_commands_system),
            )
            .add_systems(Update, broadcast::update_world_state_json)
            .add_systems(Update, broadcast::update_library_json)
            .add_systems(Update, sync_debug_log);
    }
}

#[derive(Resource)]
struct CommandSenderHolder(mpsc::Sender<commands::GameCommand>);

#[derive(Resource, Clone)]
struct RelaysHolder(AgentRelays);

#[derive(Resource, Clone)]
struct SystemRelayHolder(SystemRelay);

#[derive(Resource, Clone)]
struct WorldJsonArc(std::sync::Arc<std::sync::RwLock<String>>);

#[derive(Resource, Clone)]
pub struct LibraryJsonArc(pub std::sync::Arc<std::sync::RwLock<String>>);

#[derive(Resource, Clone)]
pub struct DebugLogArc(pub std::sync::Arc<std::sync::RwLock<Vec<ws::DebugEntry>>>);

/// Tracks total events synced using AgentEventLog::total_pushed.
#[derive(Resource, Default)]
struct DebugLogCursor {
    last_total: u64,
}

/// Path to the current run's JSONL log file on disk.
#[derive(Resource)]
struct RunLogFile(std::path::PathBuf);

/// System: sync new event log entries into the shared debug log for the REST endpoint.
fn sync_debug_log(
    event_log: Res<crate::agents::event_log::AgentEventLog>,
    holder: Res<DebugLogArc>,
    mut cursor: ResMut<DebugLogCursor>,
    suggestion_box: Res<crate::network::action_handler::SuggestionBox>,
    system_ai: Res<crate::agents::gm::SystemAiState>,
    tick: Res<crate::tick::TickCount>,
    log_file: Res<RunLogFile>,
) {
    let total = event_log.total_pushed;
    let mut new_entries: Vec<ws::DebugEntry> = Vec::new();

    // Collect new event log entries using total_pushed counter.
    if total > cursor.last_total {
        let new_count = (total - cursor.last_total) as usize;
        let deque_len = event_log.entries.len();
        let start = deque_len.saturating_sub(new_count);
        for entry in event_log.entries.iter().skip(start) {
            new_entries.push(ws::DebugEntry {
                tick: entry.tick,
                agent: entry.agent.clone(),
                kind: entry.kind.as_str().to_string(),
                text: entry.text.clone(), pos: None,
            });
        }
        cursor.last_total = total;
    }

    // Collect new feedback entries.
    let guard_read = holder.0.read().unwrap();
    for suggestion in &suggestion_box.entries {
        let already_logged = guard_read.iter().any(|e| {
            e.tick == suggestion.tick && e.agent == suggestion.agent && e.kind == "feedback"
        });
        if !already_logged {
            new_entries.push(ws::DebugEntry {
                tick: suggestion.tick,
                agent: suggestion.agent.clone(),
                kind: "feedback".to_string(),
                text: suggestion.text.clone(), pos: None,
            });
        }
    }
    drop(guard_read);

    // Drain GM thinking/response entries.
    if let Some(ref gm_log_rx) = system_ai.gm_log_rx {
        if let Ok(mut rx) = gm_log_rx.try_lock() {
            while let Ok(entry) = rx.try_recv() {
                new_entries.push(ws::DebugEntry {
                    tick: tick.0,
                    agent: "SYSTEM".to_string(),
                    kind: format!("gm_{}", entry.kind),
                    text: entry.text, pos: None,
                });
            }
        }
    }

    if new_entries.is_empty() {
        return;
    }

    // Write new entries to disk (JSONL).
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_file.0)
    {
        use std::io::Write;
        for entry in &new_entries {
            if serde_json::to_writer(&mut file, entry).is_ok() {
                let _ = writeln!(file);
            }
        }
    }

    // Append to in-memory buffer.
    let mut guard = holder.0.write().unwrap();
    guard.extend(new_entries);

    // Cap in-memory at 5000 (disk has everything).
    if guard.len() > 5000 {
        let drain = guard.len() - 5000;
        guard.drain(..drain);
    }
}

/// Load valid action names from `mcp-game --list-actions`.
fn load_valid_actions() -> Vec<String> {
    let result = std::process::Command::new("./target/debug/mcp-game")
        .arg("--list-actions")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());

    match result {
        Some(actions) => {
            tracing::info!("[STARTUP] Loaded {} valid actions from mcp-game", actions.len());
            actions
        }
        None => {
            tracing::error!("[STARTUP] Failed to load actions from mcp-game --list-actions, using empty list");
            vec![]
        }
    }
}

fn spawn_axum(
    runtime: ResMut<TokioTasksRuntime>,
    broadcast_tx: Res<BroadcastTx>,
    cmd_sender: Res<CommandSenderHolder>,
    relays: Res<RelaysHolder>,
    system_relay: Res<SystemRelayHolder>,
    world_json: Res<WorldJsonArc>,
    library_json: Res<LibraryJsonArc>,
    debug_log: Res<DebugLogArc>,
    server_port: Option<Res<ws::ServerPort>>,
) {
    let documents_dir = std::env::var("DOCUMENTS_DIR").unwrap_or_else(|_| "./documents".into());
    let _ = std::fs::create_dir_all(&documents_dir);

    // Load valid actions from mcp-game at startup.
    let valid_actions = load_valid_actions();

    let port = server_port.map(|p| p.0).unwrap_or(8080);

    let state = AppState {
        broadcast_tx: broadcast_tx.sender.clone(),
        command_tx: cmd_sender.0.clone(),
        stripe_secret: std::env::var("STRIPE_SECRET_KEY").ok(),
        agent_relays: relays.0.clone(),
        system_relay: system_relay.0.clone(),
        world_state_json: world_json.0.clone(),
        library_json: library_json.0.clone(),
        valid_actions: std::sync::Arc::new(valid_actions),
        debug_log: debug_log.0.clone(),
        documents_dir,
    };

    runtime.spawn_background_task(move |_ctx| async move {
        ws::start_server_on_port(state, port).await;
    });
}
