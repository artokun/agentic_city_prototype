//! System that generates Claude-powered dialogue when agents are chatting.

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::sync::{Arc, Mutex};

use crate::agents::ai::AgentSessions;
use crate::agents::claude;
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::social::{ChatMessage, ChattingWith};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;

/// Pending chat response for an agent.
#[derive(Component)]
pub struct PendingChatResponse(pub Arc<Mutex<Option<String>>>);

/// System: send chat prompts to Claude for agents in conversations.
pub fn ai_chat_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    runtime: ResMut<TokioTasksRuntime>,
    mut event_log: ResMut<AgentEventLog>,
    sessions: Res<AgentSessions>,
    mut chatters: Query<(
        Entity, &AgentName, &Needs, &Inventory,
        &mut ChattingWith, Option<&PendingChatResponse>,
    )>,
    agent_names: Query<&AgentName>,
) {
    for (entity, name, needs, inv, mut chat, pending) in &mut chatters {
        if !chat.needs_response { continue; }

        // Check for pending response.
        if let Some(pending) = pending {
            let maybe_response = pending.0.lock().unwrap().take();
            if let Some(dialogue) = maybe_response {
                let dialogue = extract_dialogue(&dialogue);
                tracing::info!("[Chat:{}] \"{}\"", name.0, dialogue);

                chat.messages.push(ChatMessage {
                    tick: tick.0,
                    speaker: name.0.clone(),
                    text: dialogue.clone(),
                });
                chat.needs_response = false;

                event_log.push(LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: LogKind::Speech,
                    text: dialogue,
                });

                commands.entity(entity).remove::<PendingChatResponse>();
            }
            continue; // Still waiting.
        }

        // Build chat prompt.
        let partner_name = agent_names.get(chat.partner)
            .map(|n| n.0.clone()).unwrap_or_else(|_| "someone".into());
        let gold = inv.count(ItemType::GoldCoin);

        let prompt = format!(
            "You just bumped into {}. Say something to them — be brief (1-2 sentences max). \
             Stay in character. Just respond with what you'd say, no JSON.\n\
             Your state: energy={:.0}, hunger={:.0}, boredom={:.0}, gold={}.",
            partner_name, needs.energy, needs.hunger, needs.boredom, gold,
        );

        let system_prompt = sessions.sessions.get(&entity)
            .map(|s| s.system_prompt.clone())
            .unwrap_or_default();

        let slot = Arc::new(Mutex::new(None::<String>));
        let slot_clone = slot.clone();
        let agent_name = name.0.clone();

        runtime.spawn_background_task(move |_ctx| async move {
            match claude::ask_claude(&prompt, &system_prompt).await {
                Ok(response) => {
                    tracing::debug!("[ChatClaude:{}] {}", agent_name, response);
                    *slot_clone.lock().unwrap() = Some(response);
                }
                Err(e) => {
                    tracing::error!("[ChatClaude:{}] error: {}", agent_name, e);
                    *slot_clone.lock().unwrap() = Some("Hey! *waves*".to_string());
                }
            }
        });

        commands.entity(entity).insert(PendingChatResponse(slot));
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
