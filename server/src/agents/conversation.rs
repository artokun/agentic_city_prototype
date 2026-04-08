//! Persistent face-to-face conversations between agents.
//! Agents can start, speak in, and end conversations via MCP actions.

use bevy::prelude::*;

/// Marks an agent as being in an active face-to-face conversation.
#[derive(Component)]
#[allow(dead_code)]
pub struct ActiveConversation {
    pub partner: Entity,
    pub partner_name: String,
    pub started_tick: u64,
}

/// A single message within a conversation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConversationMessage {
    pub speaker: String,
    pub text: String,
    pub tick: u64,
}

/// Log of all messages in the current conversation, attached to both agents.
#[derive(Component, Default)]
pub struct ConversationLog {
    pub messages: Vec<ConversationMessage>,
}
