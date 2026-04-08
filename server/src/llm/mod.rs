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

/// Tracks how many events have been persisted using the event log's total_pushed counter.
/// This correctly handles same-tick events and ring buffer evictions because total_pushed
/// is monotonic and never resets, while the deque length stays capped at 100.
#[derive(Resource, Default)]
struct PersistenceCursor {
    last_total: u64,
}

/// System: persist new event log entries to the supervisor's checkpoint store.
fn persist_session_events(
    event_log: Res<crate::agents::event_log::AgentEventLog>,
    supervisor: Option<ResMut<supervisor::SessionSupervisor>>,
    mut cursor: ResMut<PersistenceCursor>,
) {
    let Some(supervisor) = supervisor else { return };

    let total = event_log.total_pushed;
    if total <= cursor.last_total {
        return;
    }

    // The deque has at most 100 entries. The new events are the last (total - last_total)
    // entries in the deque, but clamped to deque length (in case we missed more than 100).
    let new_count = (total - cursor.last_total) as usize;
    let deque_len = event_log.entries.len();
    let start = deque_len.saturating_sub(new_count);

    for entry in event_log.entries.iter().skip(start) {
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

    cursor.last_total = total;
}
