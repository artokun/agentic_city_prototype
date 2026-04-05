//! Per-agent NDJSON thinking logs.
//! Captures AI decision responses and writes them to /tmp/agent-thinking-{name}.jsonl.

use bevy::prelude::*;
use std::fs::OpenOptions;
use std::io::Write;

use crate::agents::components::AgentName;
use crate::agents::event_log::{AgentEventLog, LogKind};

/// Pending thinking log entry.
#[derive(Debug, Clone)]
pub struct ThinkingEntry {
    pub tick: u64,
    pub response: String,
}

/// Component: accumulates raw AI responses for an agent.
#[derive(Component, Default)]
pub struct ThinkingLog {
    pub entries: Vec<ThinkingEntry>,
}

/// Resource: tracks how many event log entries we've already processed.
#[derive(Resource, Default)]
pub struct ThinkingLogCursor {
    pub last_seen: usize,
}

/// System: watch the AgentEventLog for new Decision entries and populate
/// the corresponding agent's ThinkingLog.
pub fn capture_thinking_system(
    event_log: Res<AgentEventLog>,
    mut cursor: ResMut<ThinkingLogCursor>,
    mut agents: Query<(&AgentName, &mut ThinkingLog)>,
) {
    let total = event_log.entries.len();
    if cursor.last_seen >= total {
        // Event log may have been trimmed; reset cursor.
        if cursor.last_seen > total {
            cursor.last_seen = total;
        }
        return;
    }

    for entry in event_log.entries.iter().skip(cursor.last_seen) {
        if entry.kind != LogKind::Decision {
            continue;
        }
        // Find the agent with this name and push the entry.
        for (name, mut log) in &mut agents {
            if name.0 == entry.agent {
                log.entries.push(ThinkingEntry {
                    tick: entry.tick,
                    response: entry.text.clone(),
                });
                break;
            }
        }
    }

    cursor.last_seen = total;
}

/// System: drain ThinkingLog entries to per-agent NDJSON files on disk.
pub fn flush_thinking_log_system(
    mut agents: Query<(&AgentName, &mut ThinkingLog)>,
) {
    for (name, mut log) in &mut agents {
        if log.entries.is_empty() {
            continue;
        }

        let path = format!("/tmp/agent-thinking-{}.jsonl", name.0.to_lowercase());
        let entries: Vec<ThinkingEntry> = log.entries.drain(..).collect();

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                for entry in &entries {
                    let json = serde_json::json!({
                        "tick": entry.tick,
                        "agent": name.0,
                        "response": entry.response,
                    });
                    if let Err(e) = writeln!(file, "{}", json) {
                        tracing::warn!("Failed to write thinking log for {}: {}", name.0, e);
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to open thinking log {}: {}", path, e);
            }
        }
    }
}
