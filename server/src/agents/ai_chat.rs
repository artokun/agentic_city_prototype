//! Generates Claude-powered dialogue when agents are chatting.
//! Uses the persistent --sdk-url session to generate in-character responses.

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::sync::{Arc, Mutex};

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::social::{ChatMessage, ChattingWith};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;

/// System: send chat prompts via the persistent session for agents in conversations.
pub fn ai_chat_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut sessions: ResMut<AgentSessions>,
    mut chatters: Query<(
        Entity, &AgentName, &Needs, &Inventory, &mut ChattingWith,
    )>,
    agent_names: Query<&AgentName>,
) {
    for (entity, name, needs, inv, mut chat) in &mut chatters {
        if !chat.needs_response { continue; }

        let Some(session) = sessions.sessions.get_mut(&entity) else { continue };

        // Check for pending response.
        if session.pending {
            let maybe_response = {
                let mut rx = session.response_rx.lock().unwrap();
                rx.try_recv().ok()
            };

            if let Some(response_text) = maybe_response {
                session.pending = false;
                let dialogue = extract_dialogue(&response_text);
                tracing::info!("[Chat:{}] \"{}\"", name.0, dialogue);

                chat.messages.push(ChatMessage {
                    tick: tick.0, speaker: name.0.clone(), text: dialogue.clone(),
                });
                chat.needs_response = false;

                event_log.push(LogEvent {
                    tick: tick.0, agent: name.0.clone(),
                    kind: LogKind::Speech, text: dialogue,
                });
            }
            continue;
        }

        // Build chat prompt and send via the persistent session.
        let partner_name = agent_names.get(chat.partner)
            .map(|n| n.0.clone()).unwrap_or_else(|_| "someone".into());
        let gold = inv.count(ItemType::GoldCoin);

        let prompt = format!(
            "You just bumped into {}. Say something to them — be brief (1-2 sentences max). \
             Stay in character. Just respond with what you'd say, no JSON.\n\
             Your state: energy={:.0}, hunger={:.0}, boredom={:.0}, gold={}.",
            partner_name, needs.energy, needs.hunger, needs.boredom, gold,
        );

        if let Err(_) = session.prompt_tx.try_send(prompt) {
            continue;
        }
        session.pending = true;
    }
}

fn extract_dialogue(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            for key in &["text", "message", "thought", "dialogue"] {
                if let Some(t) = val.get(*key).and_then(|t| t.as_str()) {
                    return t.to_string();
                }
            }
        }
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return trimmed[1..trimmed.len() - 1].to_string();
    }
    trimmed.to_string()
}
