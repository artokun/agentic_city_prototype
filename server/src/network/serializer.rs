use bevy::prelude::*;
use bytes::Bytes;
use flatbuffers::FlatBufferBuilder;

use crate::agents::actions::ActionTimer;
use crate::agents::components::*;
use crate::agents::conversation::{ActiveConversation, ConversationLog};
use crate::agents::needs::Needs;
use crate::agents::event_log::AgentEventLog;
use crate::agents::perception::{KnownLocations, Tracking, Vision};
use crate::agents::social::Relationships;
use crate::agents::token_tracking::{AgentCost, ContextWindow};
use crate::items::{Inventory, ItemType};
use crate::tick::TickCount;
use crate::world::bounty::*;
use crate::world::map::GridPos;
use crate::world::structures::*;

use schemas::world as fb;

pub fn serialize_world(
    tick: &TickCount,
    agents: &Query<(
        Entity, &AgentId, &AgentName, &GridPos, &AgentAnimation, &ThoughtBubble,
        &Inventory, &AgentGoal, &Needs, &Relationships, &Vision, &Tracking, &KnownLocations,
        Option<&ActionTimer>, Option<&ActiveConversation>,
    )>,
    agent_extras: &Query<(Option<&ConversationLog>, Option<&BusinessCards>, Option<&ContextWindow>, Option<&AgentCost>, Option<&crate::items::DocumentInventory>)>,
    structures: &Query<
        (&StructureId, &GridPos, &SpriteType, Option<&Interactable>, &Inventory, &Entrance),
        Without<AgentName>,
    >,
    bounty_registry: &BountyTokenStore,
    board_queues: &Query<&BoardQueue, With<BountyBoard>>,
    agent_names: &Query<&AgentName>,
    event_log: &AgentEventLog,
) -> Bytes {
    let mut fbb = FlatBufferBuilder::with_capacity(16384);

    let agent_offsets: Vec<_> = agents
        .iter()
        .map(|(entity, id, name, pos, anim, thought, inv, goal, needs, rels, vision, tracking, known_locs, action, active_convo)| {
            let (convo_log, cards, ctx_window, agent_cost, doc_inv) = agent_extras.get(entity).unwrap_or((None, None, None, None, None));
            let id_str = fbb.create_string(&id.0.to_string());
            let name_str = fbb.create_string(&name.0);
            let thought_str = fbb.create_string(&thought.0);
            let goal_str = fbb.create_string(&format!("{:?}", goal));

            let action_str = action.map(|a| fbb.create_string(&a.action_name));
            let action_ticks = action.map(|a| a.remaining_ticks).unwrap_or(0);

            let mut inv_slots = serialize_inventory(&mut fbb, inv);

            // Add business cards as virtual inventory items.
            if let Some(c) = cards {
                for (contact_name, _) in &c.contacts {
                    let item_str = fbb.create_string(&format!("card:{}", contact_name));
                    inv_slots.push(fb::InventorySlot::create(&mut fbb, &fb::InventorySlotArgs {
                        item_type: Some(item_str), count: 1,
                    }));
                }
            }

            // Add named documents from DocumentInventory.
            if let Some(docs) = doc_inv {
                for title in docs.documents.keys() {
                    let item_str = fbb.create_string(&format!("doc:{}", title));
                    inv_slots.push(fb::InventorySlot::create(&mut fbb, &fb::InventorySlotArgs {
                        item_type: Some(item_str), count: 1,
                    }));
                }
            }

            // Show bounty token with its actual name instead of generic "bounty_token".
            // Look up the agent's active bounty from the store.
            if let Some(bounty) = bounty_registry.tokens.values()
                .find(|b| b.claimed_by == Some(entity) && b.state == crate::world::bounty::BountyState::Claimed)
            {
                let item_str = fbb.create_string(&format!("bounty: {}", bounty.description));
                inv_slots.push(fb::InventorySlot::create(&mut fbb, &fb::InventorySlotArgs {
                    item_type: Some(item_str), count: 1,
                }));
            }

            let inv_vec = fbb.create_vector(&inv_slots);

            // Relationships.
            let rel_offsets: Vec<_> = rels.known.values().map(|mem| {
                let name = fbb.create_string(&mem.name);
                let goal = fbb.create_string(&mem.last_known_goal);
                fb::RelationshipSnapshot::create(&mut fbb, &fb::RelationshipSnapshotArgs {
                    agent_name: Some(name),
                    friendship: mem.friendship,
                    last_goal: Some(goal),
                })
            }).collect();
            let rels_vec = fbb.create_vector(&rel_offsets);

            // Active chat messages from ConversationLog.
            let chat_offsets: Vec<_> = convo_log.map(|log| {
                log.messages.iter().map(|m| {
                    let speaker = fbb.create_string(&m.speaker);
                    let text = fbb.create_string(&m.text);
                    fb::ChatMessageSnapshot::create(&mut fbb, &fb::ChatMessageSnapshotArgs {
                        speaker: Some(speaker),
                        text: Some(text),
                    })
                }).collect::<Vec<_>>()
            }).unwrap_or_default();
            let chat_vec = fbb.create_vector(&chat_offsets);

            let fb_pos = fb::Vec2i::new(pos.x, pos.y);
            let fb_anim = match anim.0 {
                AnimState::Idle => fb::AnimState::Idle,
                AnimState::Walking => fb::AnimState::Walking,
                AnimState::Working => fb::AnimState::Working,
            };
            // Vision.
            let vis_offsets: Vec<_> = vision.visible.iter().map(|v| {
                let name = fbb.create_string(&v.name);
                let kind = fbb.create_string(&format!("{:?}", v.kind));
                let pos = fb::Vec2i::new(v.pos.x, v.pos.y);
                fb::VisibleEntitySnapshot::create(&mut fbb, &fb::VisibleEntitySnapshotArgs {
                    name: Some(name), pos: Some(&pos), kind: Some(kind), distance: v.distance,
                })
            }).collect();
            let vis_vec = fbb.create_vector(&vis_offsets);

            // Tracking.
            let track_offsets: Vec<_> = tracking.targets.values().map(|t| {
                let name = fbb.create_string(&t.name);
                let pos = fb::Vec2i::new(t.last_known_pos.x, t.last_known_pos.y);
                fb::TrackedEntitySnapshot::create(&mut fbb, &fb::TrackedEntitySnapshotArgs {
                    name: Some(name), pos: Some(&pos), in_range: t.in_range,
                })
            }).collect();
            let track_vec = fbb.create_vector(&track_offsets);

            let gold = inv.count(crate::items::ItemType::GoldCoin);
            let fb_needs = fb::AgentNeeds::new(needs.energy, needs.hunger, needs.boredom);

            // Agent stats (token tracking).
            let stats = if let (Some(cw), Some(ac)) = (ctx_window, agent_cost) {
                Some(fb::AgentStats::create(&mut fbb, &fb::AgentStatsArgs {
                    tokens_used: cw.tokens_used,
                    context_limit: cw.context_limit,
                    total_cost_usd: ac.total_cost_usd,
                    session_cost_usd: ac.session_cost_usd,
                }))
            } else {
                None
            };

            fb::AgentSnapshot::create(&mut fbb, &fb::AgentSnapshotArgs {
                id: Some(id_str),
                name: Some(name_str),
                pos: Some(&fb_pos),
                animation: fb_anim,
                thought: Some(thought_str),
                inventory: Some(inv_vec),
                gold,
                goal: Some(goal_str),
                needs: Some(&fb_needs),
                current_action: action_str,
                action_ticks_left: action_ticks,
                relationships: Some(rels_vec),
                active_chat: Some(chat_vec),
                visible_entities: Some(vis_vec),
                tracked_entities: Some(track_vec),
                known_location_count: known_locs.locations.len() as u32,
                stats,
            })
        })
        .collect();
    let agents_vec = fbb.create_vector(&agent_offsets);

    let struct_offsets: Vec<_> = structures.iter().map(|(id, pos, sprite, interactable, inv, _)| {
        let id_str = fbb.create_string(&id.0.to_string());
        let sprite_str = fbb.create_string(&sprite.0);
        let fb_pos = fb::Vec2i::new(pos.x, pos.y);
        let inv_slots = serialize_inventory(&mut fbb, inv);
        let inv_vec = fbb.create_vector(&inv_slots);
        fb::StructureSnapshot::create(&mut fbb, &fb::StructureSnapshotArgs {
            id: Some(id_str), pos: Some(&fb_pos), sprite_type: Some(sprite_str),
            interactable: interactable.is_some(), inventory: Some(inv_vec),
        })
    }).collect();
    let structures_vec = fbb.create_vector(&struct_offsets);

    let bounty_offsets: Vec<_> = bounty_registry.tokens.values()
        .filter(|b| b.state != BountyState::Completed)
        .map(|b| {
        let id_str = fbb.create_string(&b.id.to_string());
        let desc_str = fbb.create_string(&b.description);
        let claimed_str = b.claimed_by.and_then(|e| agent_names.get(e).ok().map(|n| fbb.create_string(&n.0)));
        let fb_state = match b.state {
            BountyState::Available => fb::BountyStatus::Available,
            BountyState::Claimed => fb::BountyStatus::Claimed,
            BountyState::PendingVerification => fb::BountyStatus::Submitted,
            BountyState::Completed => fb::BountyStatus::Completed,
        };
        let fb_pos = fb::Vec2i::new(0, 0);
        fb::BountySnapshot::create(&mut fbb, &fb::BountySnapshotArgs {
            id: Some(id_str), description: Some(desc_str), target_pos: Some(&fb_pos),
            reward_gold: b.reward_gold, state: fb_state, claimed_by: claimed_str,
        })
    }).collect();
    let bounties_vec = fbb.create_vector(&bounty_offsets);

    let board_queue = board_queues.iter().next().map(|queue| {
        let interacting_str = queue.interacting.and_then(|e| agent_names.get(e).ok().map(|n| fbb.create_string(&n.0)));
        let waiting_strs: Vec<_> = queue.waiting.iter()
            .filter_map(|e| agent_names.get(*e).ok().map(|n| fbb.create_string(&n.0))).collect();
        let waiting_vec = fbb.create_vector(&waiting_strs);
        fb::BoardQueueSnapshot::create(&mut fbb, &fb::BoardQueueSnapshotArgs {
            interacting: interacting_str, waiting: Some(waiting_vec),
        })
    });

    // Event log.
    let log_offsets: Vec<_> = event_log.entries.iter().map(|e| {
        let agent = fbb.create_string(&e.agent);
        let kind = fbb.create_string(e.kind.as_str());
        let text = fbb.create_string(&e.text);
        fb::LogEntrySnapshot::create(&mut fbb, &fb::LogEntrySnapshotArgs {
            tick: e.tick, agent: Some(agent), kind: Some(kind), text: Some(text),
        })
    }).collect();
    let log_vec = fbb.create_vector(&log_offsets);

    let world = fb::WorldSnapshot::create(&mut fbb, &fb::WorldSnapshotArgs {
        tick: tick.0, agents: Some(agents_vec), structures: Some(structures_vec),
        bounties: Some(bounties_vec), board_queue, event_log: Some(log_vec),
    });

    fbb.finish(world, None);
    Bytes::copy_from_slice(fbb.finished_data())
}

fn serialize_inventory<'a, A: flatbuffers::Allocator + 'a>(
    fbb: &mut FlatBufferBuilder<'a, A>, inv: &Inventory,
) -> Vec<flatbuffers::WIPOffset<fb::InventorySlot<'a>>> {
    inv.items.iter()
        // Skip items shown as named variants elsewhere.
        .filter(|(item, count)| **item != ItemType::Document && **item != ItemType::BountyToken && **count > 0)
        .map(|(item, count)| {
            let item_str = fbb.create_string(&item.to_string());
            fb::InventorySlot::create(fbb, &fb::InventorySlotArgs { item_type: Some(item_str), count: *count })
        }).collect()
}
