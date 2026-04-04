//! System that generates Claude-powered dialogue when agents are chatting.

use bevy::prelude::*;
use std::sync::{Arc, Mutex};

use crate::agents::ai::AgentSessions;
use crate::agents::claude;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::social::{ChatMessage, ChattingWith};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;

/// System: send chat prompts to Claude for agents that are in a conversation.
pub fn ai_chat_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut sessions: ResMut<AgentSessions>,
    mut chatters: Query<(
        Entity,
        &AgentName,
        &Needs,
        &Inventory,
        &mut ChattingWith,
    )>,
    agent_names: Query<&AgentName>,
) {
    // Collect agents that need to generate a chat response.
    let mut pending: Vec<(Entity, String, String, String)> = Vec::new();

    for (entity, name, needs, inv, chat) in &chatters {
        if !chat.needs_response {
            continue;
        }

        let partner_name = agent_names
            .get(chat.partner)
            .map(|n| n.0.clone())
            .unwrap_or_else(|_| "someone".into());

        let gold = inv.count(ItemType::GoldCoin);

        // Build a chat prompt.
        let context = format!(
            "You just bumped into {}. Say something to them — be brief (1-2 sentences max). \
             Stay in character.\n\
             Your state: energy={:.0}, hunger={:.0}, boredom={:.0}, gold={}.\n\
             Recent conversation so far:\n{}",
            partner_name,
            needs.energy, needs.hunger, needs.boredom, gold,
            chat.messages.iter()
                .map(|m| format!("{}: {}", m.speaker, m.text))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        pending.push((entity, name.0.clone(), partner_name, context));
    }

    for (entity, agent_name, partner_name, context) in pending {
        let Some(session) = sessions.sessions.get_mut(&entity) else {
            continue;
        };

        // Check for a response from a previous chat prompt.
        if session.pending {
            let response = {
                let mut rx = session.response_rx.lock().unwrap();
                claude::drain_response(&mut rx)
            };

            if let Some(response_text) = response {
                session.pending = false;

                // Extract just the dialogue — strip JSON wrapper if present.
                let dialogue = extract_dialogue(&response_text);

                tracing::info!("[Chat:{}] \"{}\"", agent_name, dialogue);

                // Add to conversation messages.
                if let Ok((_, _, _, _, mut chat)) = chatters.get_mut(entity) {
                    chat.messages.push(ChatMessage {
                        tick: tick.0,
                        speaker: agent_name.clone(),
                        text: dialogue.clone(),
                    });
                    chat.needs_response = false;
                }

                event_log.push(LogEvent {
                    tick: tick.0,
                    agent: agent_name,
                    kind: LogKind::Speech,
                    text: dialogue,
                });
            }
            continue;
        }

        // Send the chat prompt.
        if let Err(_) = session.prompt_tx.try_send(context) {
            continue;
        }
        session.pending = true;
    }
}

fn extract_dialogue(text: &str) -> String {
    let trimmed = text.trim();

    // If it's JSON, try to extract a "text" or "message" field.
    if trimmed.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(t) = val.get("text").and_then(|t| t.as_str()) {
                return t.to_string();
            }
            if let Some(t) = val.get("message").and_then(|t| t.as_str()) {
                return t.to_string();
            }
            if let Some(t) = val.get("thought").and_then(|t| t.as_str()) {
                return t.to_string();
            }
        }
    }

    // Strip quotes if the whole thing is quoted.
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return trimmed[1..trimmed.len() - 1].to_string();
    }

    trimmed.to_string()
}
