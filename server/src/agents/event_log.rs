use bevy::prelude::*;
use std::collections::VecDeque;

/// Central event log for agent activities. Streamed to the debug monitor.
#[derive(Resource, Default)]
pub struct AgentEventLog {
    pub entries: VecDeque<LogEvent>,
}

impl AgentEventLog {
    pub fn push(&mut self, event: LogEvent) {
        self.entries.push_back(event);
        // Keep last 100 entries.
        while self.entries.len() > 100 {
            self.entries.pop_front();
        }
    }

    /// Drain new entries since last read.
    pub fn recent(&self, since_index: usize) -> &[LogEvent] {
        if since_index >= self.entries.len() {
            return &[];
        }
        let start = since_index.saturating_sub(0);
        &self.entries.as_slices().0[start..]
    }
}

#[derive(Debug, Clone)]
pub struct LogEvent {
    pub tick: u64,
    pub agent: String,
    pub kind: LogKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogKind {
    /// Internal thought/reasoning (italics).
    Thought,
    /// Action/tool call (bold).
    Action,
    /// Spoken dialogue in conversation (quoted).
    Speech,
    /// System event (discovery, arrival, etc).
    System,
    /// AI decision result.
    Decision,
    /// Game Master verdict on a bounty.
    GmVerdict,
    /// Game Master thinking/reasoning.
    GmThinking,
}

impl LogKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogKind::Thought => "thought",
            LogKind::Action => "action",
            LogKind::Speech => "speech",
            LogKind::System => "system",
            LogKind::Decision => "decision",
            LogKind::GmVerdict => "gm_verdict",
            LogKind::GmThinking => "gm_thinking",
        }
    }
}
