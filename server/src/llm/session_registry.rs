//! Bevy Resource that maps session owners to their communication handles.
//! Replaces ad-hoc HashMap<Entity, SessionState> patterns.

use bevy::prelude::*;
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::types::{SessionCommand, SessionEvent, SessionOwner};

/// Handle for communicating with a running session.
pub struct SessionHandle {
    pub command_tx: mpsc::Sender<SessionCommand>,
    pub event_rx: mpsc::Receiver<SessionEvent>,
    /// Profile name from config/llm.toml this session was started with.
    pub profile_name: String,
}

/// Central registry of all active LLM sessions.
#[derive(Resource, Default)]
pub struct SessionRegistry {
    handles: HashMap<SessionOwner, SessionHandle>,
}

impl SessionRegistry {
    /// Register a new session. Returns the old handle if one existed.
    pub fn register(
        &mut self,
        owner: SessionOwner,
        handle: SessionHandle,
    ) -> Option<SessionHandle> {
        self.handles.insert(owner, handle)
    }

    /// Get a mutable reference to a session handle.
    pub fn get_handle(&mut self, owner: &SessionOwner) -> Option<&mut SessionHandle> {
        self.handles.get_mut(owner)
    }

    /// Remove a session, returning its handle.
    pub fn remove(&mut self, owner: &SessionOwner) -> Option<SessionHandle> {
        self.handles.remove(owner)
    }

    /// List all active session owners.
    pub fn list_active(&self) -> Vec<&SessionOwner> {
        self.handles.keys().collect()
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether any sessions are registered.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}
