//! Session lifecycle supervisor — start, monitor, restart sessions.
//! Manages adapter lifecycle and integrates with durable persistence.

use std::collections::HashMap;

use super::config::SessionProfile;
use super::persistence::CheckpointStore;
use super::session_registry::SessionHandle;
use super::types::{AdapterError, SessionCheckpoint, SessionCommand, SessionEvent, SessionOwner};
use tokio::sync::mpsc;

/// Trait for provider-specific session adapters.
/// Each provider (CLI-based, API-based, etc.) implements this to translate
/// between the unified SessionCommand/SessionEvent protocol and its own wire format.
///
/// Uses async_trait for object safety with async methods.
#[async_trait::async_trait]
pub trait SessionAdapter: Send + 'static {
    /// Start a new session (or resume from checkpoint).
    async fn start(
        &mut self,
        profile: &SessionProfile,
        checkpoint: Option<&SessionCheckpoint>,
    ) -> Result<(), AdapterError>;

    /// Send a command to the running session.
    async fn send_command(&self, cmd: SessionCommand) -> Result<(), AdapterError>;

    /// Get the event receiver for this session.
    /// Called once after start() to wire into the engine.
    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<SessionEvent>>;

    /// Shut down gracefully, returning a checkpoint for later resume.
    async fn shutdown(&mut self) -> Result<Option<SessionCheckpoint>, AdapterError>;
}

/// Create paired channels for a session.
/// Returns: (handle for the registry, command_rx for the adapter, event_tx for the adapter).
/// The adapter reads commands from `command_rx` and writes events to `event_tx`.
pub fn create_handle_channels(
    profile_name: &str,
) -> (SessionHandle, mpsc::Receiver<SessionCommand>, mpsc::Sender<SessionEvent>) {
    let (command_tx, command_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let handle = SessionHandle {
        command_tx,
        event_rx,
        profile_name: profile_name.to_string(),
    };
    (handle, command_rx, event_tx)
}

/// Manages adapter lifecycle, persistence, and recovery.
pub struct SessionSupervisor {
    store: CheckpointStore,
    /// Cached checkpoints loaded at startup.
    checkpoints: HashMap<SessionOwner, SessionCheckpoint>,
}

impl SessionSupervisor {
    /// Create a supervisor with config-driven persistence directory.
    pub fn new() -> Self {
        let store = CheckpointStore::from_env();
        let checkpoints = store.load_all();
        if !checkpoints.is_empty() {
            tracing::info!(
                "[supervisor] loaded {} checkpoint(s) from disk",
                checkpoints.len()
            );
        }
        Self { store, checkpoints }
    }

    /// Create a supervisor with a custom store (for testing).
    pub fn with_store(store: CheckpointStore) -> Self {
        let checkpoints = store.load_all();
        Self { store, checkpoints }
    }

    /// Get a cached checkpoint for a session owner, if one exists.
    pub fn get_checkpoint(&self, owner: &SessionOwner) -> Option<&SessionCheckpoint> {
        self.checkpoints.get(owner)
    }

    /// Save a checkpoint to disk and update the cache.
    pub fn save_checkpoint(&mut self, checkpoint: SessionCheckpoint) {
        if let Err(e) = self.store.save(&checkpoint) {
            tracing::error!(
                "[supervisor] failed to save checkpoint for {}: {e}",
                checkpoint.owner
            );
            return;
        }
        self.checkpoints.insert(checkpoint.owner.clone(), checkpoint);
    }

    /// Save a checkpoint after compaction: updates token counters and clears the event log.
    pub fn save_after_compaction(
        &mut self,
        owner: &SessionOwner,
        compacted_context: Option<String>,
        total_input_tokens: u32,
        total_output_tokens: u32,
        total_cost_usd: f64,
    ) {
        if let Some(cp) = self.checkpoints.get_mut(owner) {
            cp.total_input_tokens = total_input_tokens;
            cp.total_output_tokens = total_output_tokens;
            cp.total_cost_usd = total_cost_usd;
            cp.compacted_context = compacted_context;

            if let Err(e) = self.store.save(cp) {
                tracing::error!(
                    "[supervisor] failed to save post-compaction checkpoint for {}: {e}",
                    owner
                );
                return;
            }

            // Truncate the event log since we've compacted.
            if let Err(e) = self.store.truncate_events(owner) {
                tracing::warn!(
                    "[supervisor] failed to truncate event log for {}: {e}",
                    owner
                );
            }

            tracing::info!("[supervisor] saved post-compaction checkpoint for {}", owner);
        } else {
            tracing::warn!(
                "[supervisor] no cached checkpoint for {} — cannot save after compaction",
                owner
            );
        }
    }

    /// Remove a checkpoint (e.g. when a session is permanently ended).
    pub fn remove_checkpoint(&mut self, owner: &SessionOwner) {
        self.checkpoints.remove(owner);
        if let Err(e) = self.store.remove(owner) {
            tracing::warn!(
                "[supervisor] failed to remove checkpoint for {}: {e}",
                owner
            );
        }
    }

    /// Append an event to the persistent event log for a session.
    pub fn log_event(
        &self,
        owner: &SessionOwner,
        event: &SessionEvent,
    ) {
        let persisted = super::persistence::session_event_to_persisted(event);
        if let Err(e) = self.store.append_event(owner, &persisted) {
            tracing::warn!(
                "[supervisor] failed to log event for {}: {e}",
                owner
            );
        }
    }

    /// Get a reference to the underlying checkpoint store.
    pub fn store(&self) -> &CheckpointStore {
        &self.store
    }
}
