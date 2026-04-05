//! Thought bubble summarizer.
//! Reads the last Thought event from the event log for each agent and truncates
//! it to a short string for the thought bubble. This is a placeholder — a future
//! version will use Claude to generate personality-flavored summaries.

use bevy::prelude::*;

use crate::agents::components::{AgentName, ThoughtBubble};
use crate::agents::event_log::{AgentEventLog, LogKind};

/// System: summarize the agent's latest thought into a short bubble (max 60 chars).
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

        // Only update if the thought text differs from current bubble content
        // (avoid overwriting action-set bubbles with stale thoughts).
        let prefix: String = entry.text.chars().take(10).collect();
        if thought.0 == entry.text || thought.0.starts_with(&prefix) {
            continue;
        }

        // Truncate to 60 chars with ellipsis (char-safe for multi-byte UTF-8).
        let char_count = entry.text.chars().count();
        if char_count <= 60 {
            thought.0 = entry.text.clone();
        } else {
            let truncated: String = entry.text.chars().take(57).collect();
            // Avoid splitting mid-word: find last space.
            let end = truncated.rfind(' ').unwrap_or(truncated.len());
            thought.0 = format!("{}...", &truncated[..end]);
        }
    }
}
