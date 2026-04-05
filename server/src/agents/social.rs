use bevy::prelude::*;
use std::collections::HashMap;

use super::actions::ActionTimer;
use super::components::*;
use super::event_log::{AgentEventLog, LogEvent, LogKind};
use super::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::map::GridPos;
use crate::world::services::ServiceEffects;

const CHAT_DURATION: u32 = 15;
const CHAT_BOREDOM_BOOST: f32 = 20.0;
const CHAT_RANGE: i32 = 2;

/// Tracks an agent's relationships with other agents.
#[derive(Component, Default, Debug)]
pub struct Relationships {
    pub known: HashMap<Entity, AgentMemory>,
}

/// What an agent remembers about another agent.
#[derive(Debug, Clone)]
pub struct AgentMemory {
    pub name: String,
    pub friendship: u32,
    pub last_seen_tick: u64,
    pub last_seen_pos: GridPos,
    pub last_known_goal: String,
    pub last_known_gold: u32,
    pub notes: Vec<String>,
    pub conversation_log: Vec<ChatMessage>,
}

/// A single message in a conversation.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub tick: u64,
    pub speaker: String,
    pub text: String,
}

/// Active conversation state.
#[derive(Component)]
pub struct ChattingWith {
    pub partner: Entity,
    pub messages: Vec<ChatMessage>,
    /// If true, Claude hasn't generated this agent's dialogue yet.
    pub needs_response: bool,
}

/// System: find pairs of nearby idle/bored agents and start conversations.
pub fn social_matchmaking_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &AgentGoal,
        &Needs,
        &Inventory,
        Option<&ActionTimer>,
        Option<&ChattingWith>,
    )>,
) {
    let available: Vec<_> = agents
        .iter()
        .filter(|(_, _, _, goal, needs, _, timer, chatting)| {
            timer.is_none()
                && chatting.is_none()
                && needs.boredom < 50.0
                && matches!(
                    goal,
                    AgentGoal::Idle | AgentGoal::Wandering | AgentGoal::WaitingAtBoard
                        | AgentGoal::WorkingShift { .. }
                )
        })
        .map(|(e, name, pos, goal, needs, inv, _, _)| {
            (e, name.0.clone(), *pos, format!("{:?}", goal), inv.count(ItemType::GoldCoin), needs.clone())
        })
        .collect();

    let mut matched = std::collections::HashSet::new();
    for i in 0..available.len() {
        if matched.contains(&i) { continue; }
        for j in (i + 1)..available.len() {
            if matched.contains(&j) { continue; }
            let dist = (available[i].2.x - available[j].2.x).abs()
                + (available[i].2.y - available[j].2.y).abs();

            if dist <= CHAT_RANGE {
                matched.insert(i);
                matched.insert(j);

                let (e_a, name_a, _, _, _, _) = &available[i];
                let (e_b, name_b, _, _, _, _) = &available[j];

                tracing::info!("{} and {} start chatting", name_a, name_b);

                // Start chat — Claude will generate the actual dialogue.
                // Messages start empty; the AI system fills them in.
                commands.entity(*e_a).insert((
                    ChattingWith { partner: *e_b, messages: vec![], needs_response: true },
                    ActionTimer {
                        action_name: format!("chatting with {}", name_b),
                        remaining_ticks: CHAT_DURATION,
                        effects: ServiceEffects { boredom: CHAT_BOREDOM_BOOST, ..Default::default() },
                        gold_cost: 0,
                        paid: true,
                        consumes_item: None,
                    },
                ));

                commands.entity(*e_b).insert((
                    ChattingWith { partner: *e_a, messages: vec![], needs_response: true },
                    ActionTimer {
                        action_name: format!("chatting with {}", name_a),
                        remaining_ticks: CHAT_DURATION,
                        effects: ServiceEffects { boredom: CHAT_BOREDOM_BOOST, ..Default::default() },
                        gold_cost: 0,
                        paid: true,
                        consumes_item: None,
                    },
                ));

                break;
            }
        }
    }
}


/// System: when chat finishes, store conversation in both agents' memories.
pub fn social_memory_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &AgentGoal,
        &Inventory,
        &mut Relationships,
        Option<&ChattingWith>,
        Option<&ActionTimer>,
    )>,
) {
    // Find agents whose chat just ended (have ChattingWith but no ActionTimer).
    let finished: Vec<_> = agents
        .iter()
        .filter(|(_, _, _, _, _, _, chatting, timer)| chatting.is_some() && timer.is_none())
        .map(|(e, name, pos, goal, inv, _, chatting, _)| {
            let chat = chatting.unwrap();
            (
                e,
                name.0.clone(),
                *pos,
                format!("{:?}", goal),
                inv.count(ItemType::GoldCoin),
                chat.partner,
                chat.messages.clone(),
            )
        })
        .collect();

    for (entity, self_name, _pos, _goal, _gold, partner, messages) in &finished {
        let partner_info = agents.get(*partner).ok().map(|(_, name, pos, goal, inv, _, _, _)| {
            (name.0.clone(), *pos, format!("{:?}", goal), inv.count(ItemType::GoldCoin))
        });

        if let Some((partner_name, partner_pos, partner_goal, partner_gold)) = partner_info {
            if let Ok((_, _, _, _, _, mut rels, _, _)) = agents.get_mut(*entity) {
                let memory = rels.known.entry(*partner).or_insert_with(|| AgentMemory {
                    name: partner_name.clone(),
                    friendship: 0,
                    last_seen_tick: 0,
                    last_seen_pos: partner_pos,
                    last_known_goal: String::new(),
                    last_known_gold: 0,
                    notes: Vec::new(),
                    conversation_log: Vec::new(),
                });

                memory.friendship += 1;
                memory.last_seen_tick = tick.0;
                memory.last_seen_pos = partner_pos;
                memory.last_known_goal = partner_goal;
                memory.last_known_gold = partner_gold;

                // Append conversation to log.
                memory.conversation_log.extend(messages.iter().cloned());

                // Keep log manageable (last 20 messages per relationship).
                if memory.conversation_log.len() > 20 {
                    let drain = memory.conversation_log.len() - 20;
                    memory.conversation_log.drain(..drain);
                }

                tracing::info!(
                    "{} updated memory of {} (friendship: {}, {} total messages)",
                    self_name, partner_name, memory.friendship, memory.conversation_log.len(),
                );
            }
        }

        commands.entity(*entity).remove::<ChattingWith>();
    }
}
