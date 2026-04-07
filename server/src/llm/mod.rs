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

/// Tracks the tick of the last persisted event to avoid duplicates.
/// Using tick instead of deque position handles the ring buffer correctly —
/// new events always have ticks >= the last persisted tick.
#[derive(Resource)]
struct PersistenceCursor {
    last_tick: u64,
}

impl Default for PersistenceCursor {
    fn default() -> Self {
        Self { last_tick: 0 }
    }
}

/// System: persist new event log entries to the supervisor's checkpoint store.
/// Tracks by tick value so ring buffer evictions don't cause missed entries.
fn persist_session_events(
    event_log: Res<crate::agents::event_log::AgentEventLog>,
    supervisor: Option<ResMut<supervisor::SessionSupervisor>>,
    mut cursor: ResMut<PersistenceCursor>,
) {
    let Some(mut supervisor) = supervisor else { return };

    for entry in &event_log.entries {
        if entry.tick <= cursor.last_tick {
            continue;
        }

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
        cursor.last_tick = entry.tick;
    }
}
