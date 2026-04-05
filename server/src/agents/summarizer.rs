//! Thought bubble updater.
//! Reads the last Thought event from the event log for each agent and sets
//! it as the thought bubble content (full text, no truncation).

use bevy::prelude::*;

use crate::agents::components::{AgentName, ThoughtBubble};
use crate::agents::event_log::{AgentEventLog, LogKind};

/// System: update the agent's thought bubble with their latest thought.
/// Runs after AI decisions so it picks up the freshest thoughts.
pub fn summarize_thoughts_system(
    event_log: Res<AgentEventLog>,
    mut agents: Query<(&AgentName, &mut ThoughtBubble)>,
) {
    for (name, mut thought) in &mut agents {
        // Find the most recent Thought entry for this agent.
        let latest = event_log
            .entries
            .iter()
            .rev()
            .find(|e| e.agent == name.0 && e.kind == LogKind::Thought);

        let Some(entry) = latest else { continue };

        if thought.0 != entry.text {
            thought.0 = entry.text.clone();
        }
    }
}
