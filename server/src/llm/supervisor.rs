//! Session lifecycle supervisor — start, monitor, restart sessions.
//! Stub for Gate 0; will be filled in during later gates.

use super::config::SessionProfile;
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

/// Placeholder: will manage adapter lifecycle in later gates.
pub struct SessionSupervisor;

impl SessionSupervisor {
    pub fn new() -> Self {
        Self
    }
}
