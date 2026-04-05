//! Async mailbox system for inter-agent messaging.
//!
//! Agents within range (2 tiles) can send messages that are stored in the
//! recipient's `Mailbox` component. A delivery system periodically flushes
//! the mailbox and forwards the content to the agent's Claude session as
//! game events.

use bevy::prelude::*;

use crate::agents::ai::AgentSessions;
use crate::agents::components::*;
use crate::tick::TickCount;
use crate::world::map::GridPos;

/// Maximum tile distance for sending a message.
const SEND_RANGE: i32 = 2;

/// A single message stored in an agent's mailbox.
#[derive(Debug, Clone)]
pub struct MailMessage {
    /// Display name of the sender.
    pub sender: String,
    /// Message body.
    pub text: String,
    /// Tick when the message was sent.
    pub tick: u64,
}

/// Per-agent mailbox that accumulates incoming messages.
#[derive(Component, Default, Debug)]
pub struct Mailbox {
    pub messages: Vec<MailMessage>,
}

/// Marker component: the agent wants to send a message this tick.
/// Inserted by the AI decision pipeline, consumed by [`process_outgoing_mail_system`].
#[derive(Component)]
pub struct WantsToSendMessage {
    pub recipient_name: String,
    pub text: String,
}

/// System: process outgoing messages.
///
/// For each agent with a [`WantsToSendMessage`] component, find the named
/// recipient, verify they are within range, and push a [`MailMessage`] into
/// the recipient's [`Mailbox`]. The marker component is removed afterwards.
pub fn process_outgoing_mail_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    senders: Query<(Entity, &AgentName, &GridPos, &WantsToSendMessage)>,
    mut recipients: Query<(&AgentName, &GridPos, &mut Mailbox)>,
) {
    for (sender_entity, sender_name, sender_pos, wants) in &senders {
        let mut delivered = false;

        for (r_name, r_pos, mut r_mailbox) in &mut recipients {
            if r_name.0 != wants.recipient_name {
                continue;
            }

            let dist = (r_pos.x - sender_pos.x).abs() + (r_pos.y - sender_pos.y).abs();
            if dist > SEND_RANGE {
                tracing::debug!(
                    "[Mailbox] {} tried to message {} but too far ({} tiles)",
                    sender_name.0,
                    wants.recipient_name,
                    dist,
                );
                break;
            }

            r_mailbox.messages.push(MailMessage {
                sender: sender_name.0.clone(),
                text: wants.text.clone(),
                tick: tick.0,
            });

            tracing::info!(
                "[Mailbox] {} -> {}: \"{}\"",
                sender_name.0,
                wants.recipient_name,
                wants.text,
            );
            delivered = true;
            break;
        }

        if !delivered {
            tracing::debug!(
                "[Mailbox] {} could not deliver message to {}",
                sender_name.0,
                wants.recipient_name,
            );
        }

        commands.entity(sender_entity).remove::<WantsToSendMessage>();
    }
}

/// System: deliver accumulated mailbox messages to the agent's Claude session.
///
/// Each pending message is formatted as a game event string and sent via the
/// session's `prompt_tx` channel. Delivered messages are cleared from the
/// mailbox.
pub fn deliver_mail_system(
    sessions: Res<AgentSessions>,
    mut mailboxes: Query<(Entity, &AgentName, &mut Mailbox)>,
) {
    for (entity, name, mut mailbox) in &mut mailboxes {
        if mailbox.messages.is_empty() {
            continue;
        }

        let Some(session) = sessions.sessions.get(&entity) else {
            continue;
        };

        let mut lines: Vec<String> = Vec::new();
        for msg in mailbox.messages.iter() {
            lines.push(format!(
                "Message from {}: \"{}\"",
                msg.sender, msg.text,
            ));
        }

        let combined = lines.join("\n");
        if let Err(e) = session.prompt_tx.try_send(combined.clone()) {
            tracing::debug!(
                "[Mailbox:{}] failed to deliver mail: {}",
                name.0,
                e,
            );
            // Keep messages for retry next tick.
            continue;
        }

        tracing::debug!("[Mailbox:{}] delivered {} message(s)", name.0, mailbox.messages.len());
        mailbox.messages.clear();
    }
}
