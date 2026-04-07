//! Unified LLM session engine — provider-neutral types, config, and registry.

pub mod config;
pub mod persistence;
pub mod providers;
pub mod session_registry;
pub mod supervisor;
pub mod tools;
pub mod types;

use bevy::prelude::*;

/// Plugin that adds LLM session persistence systems.
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, persist_session_events);
    }
}

/// System: persist new event log entries to the supervisor's checkpoint store.
/// Also logs session lifecycle events (compaction, errors) for durable recovery.
fn persist_session_events(
    event_log: Res<crate::agents::event_log::AgentEventLog>,
    supervisor: Option<ResMut<supervisor::SessionSupervisor>>,
) {
    let Some(mut supervisor) = supervisor else { return };

    // Persist agent action/thought events for session recovery.
    for entry in &event_log.entries {
        let owner = if entry.agent == "SYSTEM" {
            types::SessionOwner::SystemAi
        } else {
            types::SessionOwner::Agent(entry.agent.clone())
        };

        let event = match entry.kind {
            crate::agents::event_log::LogKind::Thought => {
                types::SessionEvent::TextDelta(entry.text.clone())
            }
            crate::agents::event_log::LogKind::GmVerdict => {
                types::SessionEvent::TextDelta(entry.text.clone())
            }
            _ => continue, // Only persist thoughts and GM verdicts to session logs.
        };

        supervisor.log_event(&owner, &event);
    }
}
