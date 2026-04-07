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
        app.init_resource::<PersistenceCursor>()
            .add_systems(Update, persist_session_events);
    }
}

/// Tracks how many event log entries we've already persisted.
#[derive(Resource, Default)]
struct PersistenceCursor(usize);

/// System: persist new event log entries to the supervisor's checkpoint store.
/// Uses a cursor to avoid re-appending entries that were already written.
fn persist_session_events(
    event_log: Res<crate::agents::event_log::AgentEventLog>,
    supervisor: Option<ResMut<supervisor::SessionSupervisor>>,
    mut cursor: ResMut<PersistenceCursor>,
) {
    let Some(mut supervisor) = supervisor else { return };

    let current_len = event_log.entries.len();
    if cursor.0 >= current_len {
        // Event log was drained (ring buffer popped older entries) — reset cursor.
        if cursor.0 > current_len {
            cursor.0 = current_len;
        }
        return;
    }

    for entry in event_log.entries.iter().skip(cursor.0) {
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
            _ => continue,
        };

        supervisor.log_event(&owner, &event);
    }

    cursor.0 = current_len;
}
