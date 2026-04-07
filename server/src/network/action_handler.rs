//! Processes MCP game_action commands by applying them to agent ECS entities.
//! This is the authoritative bridge between Claude's tool calls and the game world.

use bevy::prelude::*;
use tokio::sync::mpsc;

use crate::agents::ai::AgentSessions;
use crate::agents::ai_decision::AgentAction;
use crate::agents::components::*;
use crate::agents::conversation::{ActiveConversation, ConversationLog, ConversationMessage};
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::mailbox::WantsToSendMessage;
use crate::agents::needs::Needs;
use crate::agents::pathfinding;
use crate::agents::perception::{KnownLocations, WantsToLook};
use crate::agents::trading::{self, TradeProposal};
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::{BountyBoard, BountyDropbox, BountyTokenStore, Library};
use crate::world::map::{GridPos, WorldMap};
use crate::world::shifts::ShiftWorker;
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

/// Pending action from an MCP tool call, waiting to be applied.
#[derive(Resource, Default)]
pub struct PendingActions {
    pub actions: Vec<MpcAction>,
}

/// Agent feedback/bug reports/suggestions — visible in debug monitor.
#[derive(Resource, Default)]
pub struct SuggestionBox {
    pub entries: Vec<Suggestion>,
}

pub struct Suggestion {
    pub tick: u64,
    pub agent: String,
    pub text: String,
}

pub struct MpcAction {
    pub agent_name: String,
    pub agent_id: String,
    pub action: String,
    pub building: Option<String>,
    pub service: Option<String>,
    pub agent_target: Option<String>,
    pub text: Option<String>,
    pub feedback: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

/// Marker component: items to give agent after claiming a bounty.
#[derive(Component)]
pub struct PendingClaimItems {
    pub items: Vec<(crate::items::ItemType, u32)>,
}

/// Marker component: queued item deposit from agent into a structure.
#[derive(Component)]
pub struct PendingDeposit {
    pub item_name: String,
    pub building_entity: Entity,
}

/// Marker component: queued item pickup from a structure into agent.
#[derive(Component)]
pub struct PendingTakeItem {
    pub item_name: String,
    pub building_entity: Entity,
}

/// Marker component: agent wants to create a document.
#[derive(Component)]
pub struct PendingCreateDocument {
    pub title: String,
    pub content: String,
}

/// Batch variant for returning multiple documents (e.g. on bounty rejection).
#[derive(Component)]
pub struct PendingCreateDocumentBatch {
    pub docs: Vec<(String, String)>, // (title, content)
}

/// Marker component: queued conversation message to apply to both agents' logs.
#[derive(Component)]
pub struct PendingConversationMessage {
    pub message: ConversationMessage,
    pub partner: Entity,
}

fn queue_dropbox_return(
    commands: &mut Commands,
    entity: Entity,
    contained_items: &mut crate::items::ContainedItems,
    slot: crate::world::bounty::DropboxSlot,
    bounty: Option<&crate::world::bounty::BountyTokenData>,
) {
    let mut return_items: Vec<(crate::items::ItemType, u32)> = Vec::new();

    if slot.bounty_token_id.is_some() {
        return_items.push((crate::items::ItemType::BountyToken, 1));
    }

    if let Some(item_entity) = slot.bounty_token_item {
        commands
            .entity(item_entity)
            .insert(crate::items::ItemContainer(entity));
        contained_items.insert(item_entity);
    }

    for (item, count) in slot.items {
        return_items.push((item, count));
    }

    for item_entity in slot.document_items {
        commands
            .entity(item_entity)
            .insert(crate::items::ItemContainer(entity));
        contained_items.insert(item_entity);
    }

    if !slot.documents.is_empty() {
        commands
            .entity(entity)
            .insert(PendingCreateDocumentBatch {
                docs: slot.documents,
            });
    }

    if !return_items.is_empty() {
        commands.entity(entity).insert(PendingClaimItems {
            items: return_items,
        });
    }
}

/// System: apply pending conversation messages to ConversationLog on both agents
/// and boost partner's boredom.
pub fn apply_conversation_messages_system(
    mut commands: Commands,
    pending: Query<(Entity, &PendingConversationMessage)>,
    mut logs: Query<&mut ConversationLog>,
    mut needs: Query<&mut Needs>,
) {
    for (entity, pending_msg) in &pending {
        // Add message to speaker's log.
        if let Ok(mut log) = logs.get_mut(entity) {
            log.messages.push(pending_msg.message.clone());
        }
        // Add message to partner's log and boost boredom.
        if let Ok(mut log) = logs.get_mut(pending_msg.partner) {
            log.messages.push(pending_msg.message.clone());
        }
        if let Ok(mut partner_needs) = needs.get_mut(pending_msg.partner) {
            partner_needs.boredom = (partner_needs.boredom + 5.0).min(100.0);
        }
        // Remove the marker.
        commands
            .entity(entity)
            .remove::<PendingConversationMessage>();
    }
}

/// System: give claim items to agents who just claimed a bounty.
pub fn give_claim_items_system(
    mut commands: Commands,
    pending: Query<(Entity, &AgentName, &PendingClaimItems)>,
    mut inventories: Query<&mut Inventory, With<AgentName>>,
    sessions: Res<crate::agents::ai::AgentSessions>,
) {
    for (entity, name, claim) in &pending {
        if let Ok(mut inv) = inventories.get_mut(entity) {
            let mut received: Vec<String> = Vec::new();
            for (item, count) in &claim.items {
                inv.add(*item, *count);
                received.push(format!("{} x{}", item, count));
                tracing::info!("[ClaimItems] {} received {} x{}", name.0, item, count);
            }
            // Notify the agent they received items.
            if !received.is_empty() {
                if let Some(session) = sessions.sessions.get(&entity) {
                    let _ = session.prompt_tx.try_send(
                        format!("You received items for this bounty: {}. These are now in your inventory. Use them to complete the bounty.", received.join(", ")),
                    );
                }
            }
        }
        commands.entity(entity).remove::<PendingClaimItems>();
    }
}

/// System: process GM verdicts — pay out approved bounties, reject others.
/// On approval: pay gold, move docs from dropbox to Library, clear dropbox slot.
/// On rejection: return all items from dropbox to agent, mark bounty Claimed (retry).
pub fn process_gm_verdicts_system(
    mut commands: Commands,
    mut verdicts: ResMut<super::commands::PendingVerdicts>,
    mut system_ai: ResMut<crate::agents::gm::SystemAiState>,
    mut boards_verdict: Query<(&mut BountyTokenStore, &mut BountyDropbox), With<BountyBoard>>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &mut Inventory,
        &mut crate::items::ContainedItems,
        &mut ThoughtBubble,
        &mut AgentGoal,
    ),
        (With<AgentName>, Without<BountyBoard>)>,
    mut board_contained_items: Query<
        &mut crate::items::ContainedItems,
        (With<BountyBoard>, Without<AgentName>),
    >,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
    sessions: Res<crate::agents::ai::AgentSessions>,
    mut library: ResMut<Library>,
) {
    let Some((mut bounty_registry, mut dropbox)) = boards_verdict.iter_mut().next() else {
        return;
    };
    if !verdicts.verdicts.is_empty() {
        tracing::info!(
            "[GM VERDICT PROCESSING] {} verdicts pending",
            verdicts.verdicts.len()
        );
    }
    for (bounty_id, approved, reason) in verdicts.verdicts.drain(..) {
        tracing::info!(
            "[GM VERDICT PROCESSING] bounty={} approved={}",
            bounty_id,
            approved
        );
        system_ai.mark_review_complete(bounty_id);
        let bounty = bounty_registry.tokens.get(&bounty_id).cloned();
        let Some(bounty) = bounty else {
            tracing::warn!("[GM] Bounty {} not found", bounty_id);
            continue;
        };
        if bounty.state != crate::world::bounty::BountyState::PendingVerification {
            tracing::warn!(
                "[GM] Ignoring stale verdict for {} in state {:?}",
                bounty_id,
                bounty.state
            );
            continue;
        }
        let Some(agent_entity) = bounty.claimed_by else {
            tracing::warn!("[GM] Bounty {} has no claimant", bounty_id);
            continue;
        };

        if approved {
            // Pay out.
            let agent_name_str;
            if let Ok((_, name, mut inv, _contained, mut thought, mut goal)) =
                agents.get_mut(agent_entity)
            {
                agent_name_str = name.0.clone();
                inv.add(crate::items::ItemType::GoldCoin, bounty.reward_gold);
                thought.0 = format!("GM approved! Collected {} gold!", bounty.reward_gold);
                *goal = AgentGoal::Idle;
                tracing::info!(
                    "[GM APPROVED] {} +{} gold for '{}'",
                    name.0,
                    bounty.reward_gold,
                    bounty.description
                );

                event_log.push(crate::agents::event_log::LogEvent {
                    tick: tick.0,
                    agent: "SYSTEM".into(),
                    kind: crate::agents::event_log::LogKind::GmVerdict,
                    text: format!("APPROVED {}'s bounty '{}' ({}g): {}", name.0, bounty.description, bounty.reward_gold, reason),
                });

                if let Some(session) = sessions.sessions.get(&agent_entity) {
                    let _ = session.prompt_tx.try_send(
                        format!("BOUNTY APPROVED by the Game Master! You earned {} gold for '{}'. Reason: {}", bounty.reward_gold, bounty.description, reason),
                    );
                }
            } else {
                agent_name_str = "unknown".into();
            }

            // Move documents from dropbox to Library and save to disk.
            if let Some(slot) = dropbox.clear_slot(agent_entity) {
                if let Ok(mut board_items) = board_contained_items.single_mut() {
                    if let Some(item_entity) = slot.bounty_token_item {
                        board_items.remove(item_entity);
                        commands.entity(item_entity).despawn();
                    }
                    for item_entity in slot.document_items {
                        board_items.remove(item_entity);
                        commands.entity(item_entity).despawn();
                    }
                }
                for (title, content) in &slot.documents {
                    library.documents.push(crate::world::bounty::LibraryEntry {
                        title: title.clone(),
                        content: content.clone(),
                        author: agent_name_str.clone(),
                        tick: tick.0,
                        bounty_description: bounty.description.clone(),
                    });
                    // Save to filesystem.
                    let agent_dir = agent_documents_dir(&agent_name_str);
                    let _ = std::fs::create_dir_all(&agent_dir);
                    let _ = std::fs::write(agent_dir.join(title), content);
                    tracing::info!(
                        "[Library] Archived '{}' by {} (bounty: {})",
                        title,
                        agent_name_str,
                        bounty.description
                    );
                }
                // Token is consumed, proof items are consumed on approval.
            }

            // Mark completed.
            if let Some(b) = bounty_registry.tokens.get_mut(&bounty_id) {
                b.state = crate::world::bounty::BountyState::Completed;
            }
        } else {
            // Rejected — return all items from dropbox to agent, keep bounty Claimed.
            if let Ok((_, name, _inv, mut contained, mut thought, _goal)) =
                agents.get_mut(agent_entity)
            {
                thought.0 = format!("GM rejected bounty: {}", reason);
                tracing::info!(
                    "[GM REJECTED] {} bounty '{}': {}",
                    name.0,
                    bounty.description,
                    reason
                );

                event_log.push(crate::agents::event_log::LogEvent {
                    tick: tick.0,
                    agent: "SYSTEM".into(),
                    kind: crate::agents::event_log::LogKind::GmVerdict,
                    text: format!("REJECTED {}'s bounty '{}': {}", name.0, bounty.description, reason),
                });

                if let Some(session) = sessions.sessions.get(&agent_entity) {
                    let _ = session.prompt_tx.try_send(
                        format!("BOUNTY REJECTED by the Game Master. Your submission for '{}' was not approved. Reason: {}. Your items have been returned — fix the issues and try again.", bounty.description, reason),
                    );
                }
            }

            // Return all items from dropbox to agent.
            if let Some(slot) = dropbox.clear_slot(agent_entity) {
                if let Ok(mut board_items) = board_contained_items.single_mut() {
                    if let Some(item_entity) = slot.bounty_token_item {
                        board_items.remove(item_entity);
                    }
                    for item_entity in &slot.document_items {
                        board_items.remove(*item_entity);
                    }
                }
                if let Ok((_, _, _, mut contained, _, _)) = agents.get_mut(agent_entity) {
                    queue_dropbox_return(
                        &mut commands,
                        agent_entity,
                        &mut contained,
                        slot,
                        Some(&bounty),
                    );
                }
            }

            // Keep bounty as Claimed — agent can retry.
            // (Don't change state; it stays Claimed with the same agent.)
        }
    }
}

/// System: deliver research documents to agents.
pub fn deliver_documents_system(
    mut commands: Commands,
    mut pending: ResMut<super::commands::PendingDocuments>,
    mut agents: Query<(Entity, &AgentName, &mut ThoughtBubble)>,
    sessions: Res<crate::agents::ai::AgentSessions>,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
) {
    for (agent_name, title, content) in pending.docs.drain(..) {
        let agent = agents.iter_mut().find(|(_, n, _)| n.0 == agent_name);
        if let Some((entity, name, mut thought)) = agent {
            commands.entity(entity).insert(PendingCreateDocument {
                title: title.clone(),
                content: content.clone(),
            });
            thought.0 = format!(
                "Research complete! Document '{}' is in your inventory.",
                title
            );
            tracing::info!(
                "[DOC] {} received '{}' ({} chars)",
                name.0,
                title,
                content.len()
            );

            event_log.push(crate::agents::event_log::LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: crate::agents::event_log::LogKind::System,
                text: format!("Research document produced: '{}'", title),
            });

            if let Some(session) = sessions.sessions.get(&entity) {
                let _ = session.prompt_tx.try_send(format!(
                    "Research complete. '{}' is now in your inventory as a document item.",
                    title
                ));
            }
        }
    }
}

/// System: process discretionary gold grants issued by the System AI.
pub fn process_gold_grants_system(
    mut pending: ResMut<super::commands::PendingGoldGrants>,
    mut agents: Query<(Entity, &AgentName, &mut Inventory, &mut ThoughtBubble)>,
    sessions: Res<crate::agents::ai::AgentSessions>,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
) {
    for (agent_name, amount, reason, message) in pending.grants.drain(..) {
        if amount == 0 {
            continue;
        }

        let Some((entity, name, mut inv, mut thought)) = agents
            .iter_mut()
            .find(|(_, name, _, _)| name.0.eq_ignore_ascii_case(&agent_name))
        else {
            tracing::warn!(
                "[GM] Couldn't grant {}g to '{}': agent not found",
                amount,
                agent_name
            );
            continue;
        };

        inv.add(crate::items::ItemType::GoldCoin, amount);
        thought.0 = format!("Received {} gold from the System AI.", amount);

        let public_message = message.unwrap_or_else(|| reason.clone());
        event_log.push(crate::agents::event_log::LogEvent {
            tick: tick.0,
            agent: "SYSTEM".into(),
            kind: crate::agents::event_log::LogKind::System,
            text: format!("[grant {}g to {}] {}", amount, name.0, public_message),
        });

        if let Some(session) = sessions.sessions.get(&entity) {
            let _ = session.prompt_tx.try_send(format!(
                "SYSTEM AI AWARD: You received {} gold. Reason: {}",
                amount, reason
            ));
        }

        tracing::info!("[GM] Granted {}g to {} ({})", amount, name.0, reason);
    }
}

/// System: process pending item deposits (agent → structure/tile transfer).
/// Special cases:
/// - "body:AgentName" at hospital → starts their recovery
/// - At the bounty board → items go into the BountyDropbox instead of building inventory
pub fn process_deposits_system(
    mut commands: Commands,
    deposits: Query<(Entity, &AgentName, &PendingDeposit)>,
    mut agent_inventories: Query<&mut Inventory, With<AgentName>>,
    mut agent_docs: Query<&mut crate::items::DocumentInventory, With<AgentName>>,
    mut contained_items: Query<&mut crate::items::ContainedItems>,
    mut structure_inventories: Query<&mut Inventory, (With<StructureId>, Without<AgentName>)>,
    mut structure_docs: Query<
        &mut crate::items::DocumentInventory,
        (With<StructureId>, Without<AgentName>),
    >,
    structures: Query<
        (
            &crate::world::structures::SpriteType,
            Option<&crate::items::RestrictedItems>,
        ),
        With<StructureId>,
    >,
    mut incapacitated_agents: Query<
        (Entity, &AgentName, &mut ThoughtBubble),
        With<crate::world::hospital::Incapacitated>,
    >,
    mut dropbox_boards: Query<&mut BountyDropbox, With<BountyBoard>>,
    board_entities: Query<Entity, With<BountyBoard>>,
    item_lookup: Query<(
        Entity,
        &crate::items::ItemKind,
        Option<&crate::items::ItemName>,
        Option<&crate::items::BountyTokenInfo>,
    )>,
    mut item_containers: Query<&mut crate::items::ItemContainer>,
) {
    // Pre-check: is the deposit target a bounty board?
    let board_entity_set: std::collections::HashSet<Entity> = board_entities.iter().collect();

    for (entity, name, deposit) in &deposits {
        let is_bounty_board = board_entity_set.contains(&deposit.building_entity);

        // Special case: depositing a body at the hospital.
        if deposit.item_name.starts_with("body:") {
            let victim_name = &deposit.item_name[5..];
            let is_hospital = structures
                .get(deposit.building_entity)
                .map(|(sprite, _)| sprite.0 == "hospital")
                .unwrap_or(false);

            if is_hospital {
                for (victim_entity, vname, mut vthought) in &mut incapacitated_agents {
                    if vname.0 == victim_name {
                        commands
                            .entity(victim_entity)
                            .insert(crate::world::hospital::Recovering {
                                ticks_remaining: crate::config::recovery_ticks(),
                            });
                        vthought.0 = format!(
                            "Being treated at the hospital... {} brought me here.",
                            name.0
                        );
                        tracing::info!(
                            "[RESCUE] {} delivered {} to the hospital — recovery started!",
                            name.0,
                            victim_name
                        );
                        break;
                    }
                }
            } else {
                tracing::info!(
                    "[Deposit] {} dropped {} here (not the hospital)",
                    name.0,
                    deposit.item_name
                );
            }
        } else if is_bounty_board {
            // --- Bounty board: route into BountyDropbox ---
            if deposit.item_name == "bounty_token" || deposit.item_name == "bountytoken" {
                // Validate: agent must actually have a BountyToken in inventory.
                let has_token = agent_inventories
                    .get(entity)
                    .map(|inv| inv.has(crate::items::ItemType::BountyToken, 1))
                    .unwrap_or(false);

                if !has_token {
                    tracing::warn!(
                        "[Dropbox] {} tried to deposit bounty_token but doesn't have one",
                        name.0
                    );
                } else {
                    let token_item = contained_items.get(entity).ok().and_then(|contained| {
                        contained.items.iter().find_map(|item_entity| {
                            item_lookup.get(*item_entity).ok().and_then(
                                |(item_entity, kind, _name, token_info)| {
                                    if kind.0 == crate::items::ItemType::BountyToken {
                                        token_info.map(|info| {
                                            let bounty_id =
                                                uuid::Uuid::parse_str(&info.bounty_id).ok()?;
                                            Some((item_entity, bounty_id))
                                        })?
                                    } else {
                                        None
                                    }
                                },
                            )
                        })
                    });

                    let bounty_id = token_item.map(|(_, bounty_id)| bounty_id);
                    if let Some(bid) = bounty_id {
                        // Remove from agent inventory.
                        if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                            agent_inv.remove(crate::items::ItemType::BountyToken, 1);
                        }
                        if let Some((item_entity, _)) = token_item {
                            if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                owner.0 = deposit.building_entity;
                            }
                            if let Ok(mut source) = contained_items.get_mut(entity) {
                                source.remove(item_entity);
                            }
                            if let Ok(mut board_items) =
                                contained_items.get_mut(deposit.building_entity)
                            {
                                board_items.insert(item_entity);
                            }
                        }
                        // Put in dropbox.
                        if let Some(mut dropbox) = dropbox_boards.iter_mut().next() {
                            dropbox.deposit_token(
                                entity,
                                bid,
                                token_item.map(|(item_entity, _)| item_entity),
                            );
                        }
                        tracing::info!(
                            "[Dropbox] {} deposited bounty_token (bounty {})",
                            name.0,
                            &bid.to_string()[..6]
                        );
                    } else {
                        tracing::warn!(
                            "[Dropbox] {} has bounty_token but no valid bounty token item metadata",
                            name.0
                        );
                    }
                }
            } else if deposit.item_name.starts_with("doc:") || deposit.item_name.ends_with(".md") {
                // Deposit document into dropbox.
                let doc_title = deposit
                    .item_name
                    .strip_prefix("doc:")
                    .unwrap_or(&deposit.item_name);
                let mut found = false;
                let document_item = contained_items.get(entity).ok().and_then(|contained| {
                    contained.items.iter().find_map(|item_entity| {
                        item_lookup.get(*item_entity).ok().and_then(
                            |(item_entity, kind, item_name, _)| {
                                (kind.0 == crate::items::ItemType::Document
                                    && item_name.is_some_and(|name| name.0 == doc_title))
                                .then_some(item_entity)
                            },
                        )
                    })
                });
                if let Ok(mut docs) = agent_docs.get_mut(entity) {
                    if let Some(content) = docs.documents.remove(doc_title) {
                        found = true;
                        if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                            agent_inv.remove(crate::items::ItemType::Document, 1);
                        }
                        if let Some(item_entity) = document_item {
                            if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                owner.0 = deposit.building_entity;
                            }
                            if let Ok(mut source) = contained_items.get_mut(entity) {
                                source.remove(item_entity);
                            }
                            if let Ok(mut board_items) =
                                contained_items.get_mut(deposit.building_entity)
                            {
                                board_items.insert(item_entity);
                            }
                        }
                        if let Some(mut dropbox) = dropbox_boards.iter_mut().next() {
                            dropbox.deposit_document(
                                entity,
                                doc_title.to_string(),
                                content.clone(),
                                document_item,
                            );
                        }
                        tracing::info!(
                            "[Dropbox] {} deposited document '{}' ({} chars)",
                            name.0,
                            doc_title,
                            content.len()
                        );
                    }
                }
                if !found {
                    tracing::info!("[Dropbox] {} doesn't have document '{}' — available docs listed in inventory", name.0, doc_title);
                }
            } else {
                // Regular item deposit into dropbox.
                let item_type = parse_item_name(&deposit.item_name);
                if let Some(item) = item_type {
                    if item == crate::items::ItemType::Document {
                        let mut deposited = false;
                        if let Ok(mut docs) = agent_docs.get_mut(entity) {
                            if let Some((title, content)) = take_named_document(&mut docs) {
                                let document_item =
                                    contained_items.get(entity).ok().and_then(|contained| {
                                        contained.items.iter().find_map(|item_entity| {
                                            item_lookup.get(*item_entity).ok().and_then(
                                                |(item_entity, kind, item_name, _)| {
                                                    (kind.0 == crate::items::ItemType::Document
                                                        && item_name
                                                            .is_some_and(|name| name.0 == title))
                                                    .then_some(item_entity)
                                                },
                                            )
                                        })
                                    });
                                if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                                    agent_inv.remove(crate::items::ItemType::Document, 1);
                                }
                                if let Some(item_entity) = document_item {
                                    if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                        owner.0 = deposit.building_entity;
                                    }
                                    if let Ok(mut source) = contained_items.get_mut(entity) {
                                        source.remove(item_entity);
                                    }
                                    if let Ok(mut board_items) =
                                        contained_items.get_mut(deposit.building_entity)
                                    {
                                        board_items.insert(item_entity);
                                    }
                                }
                                if let Some(mut dropbox) = dropbox_boards.iter_mut().next() {
                                    dropbox.deposit_document(
                                        entity,
                                        title.clone(),
                                        content.clone(),
                                        document_item,
                                    );
                                }
                                tracing::info!(
                                    "[Dropbox] {} deposited document '{}' via generic document item ({} chars)",
                                    name.0,
                                    title,
                                    content.len()
                                );
                                deposited = true;
                            }
                        }

                        if !deposited {
                            tracing::info!(
                                "[Dropbox] {} tried to deposit a generic document item but has no named documents to submit",
                                name.0
                            );
                        }
                        commands.entity(entity).remove::<PendingDeposit>();
                        continue;
                    }

                    if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                        if agent_inv.has(item, 1) {
                            agent_inv.remove(item, 1);
                            if let Some(mut dropbox) = dropbox_boards.iter_mut().next() {
                                dropbox.deposit_item(entity, item, 1);
                            }
                            tracing::info!(
                                "[Dropbox] {} deposited {} into bounty board dropbox",
                                name.0,
                                deposit.item_name
                            );
                        } else {
                            tracing::info!(
                                "[Dropbox] {} doesn't have {} in inventory",
                                name.0,
                                deposit.item_name
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        "[Dropbox] Unknown item '{}'. Check your inventory for valid item names.",
                        deposit.item_name
                    );
                }
            }
        } else if deposit.item_name.starts_with("doc:") || deposit.item_name.ends_with(".md") {
            let accepts_documents = structures
                .get(deposit.building_entity)
                .ok()
                .and_then(|(_, restricted)| restricted)
                .map_or(true, |restricted| {
                    restricted.allows(crate::items::ItemType::Document)
                });
            if !accepts_documents {
                tracing::warn!(
                    "[Deposit] {} tried to deposit a document into a restricted container",
                    name.0
                );
                commands.entity(entity).remove::<PendingDeposit>();
                continue;
            }

            // Named document deposit at a regular building.
            let doc_title = deposit
                .item_name
                .strip_prefix("doc:")
                .unwrap_or(&deposit.item_name);
            let mut found = false;
            let document_item = contained_items.get(entity).ok().and_then(|contained| {
                contained.items.iter().find_map(|item_entity| {
                    item_lookup.get(*item_entity).ok().and_then(
                        |(item_entity, kind, item_name, _)| {
                            (kind.0 == crate::items::ItemType::Document
                                && item_name.is_some_and(|name| name.0 == doc_title))
                            .then_some(item_entity)
                        },
                    )
                })
            });
            if let Ok(mut docs) = agent_docs.get_mut(entity) {
                if let Some(content) = docs.documents.remove(doc_title) {
                    found = true;
                    if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                        agent_inv.remove(crate::items::ItemType::Document, 1);
                    }
                    if let Some(item_entity) = document_item {
                        if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                            owner.0 = deposit.building_entity;
                        }
                        if let Ok(mut source) = contained_items.get_mut(entity) {
                            source.remove(item_entity);
                        }
                        if let Ok(mut destination) =
                            contained_items.get_mut(deposit.building_entity)
                        {
                            destination.insert(item_entity);
                        }
                    }
                    if let Ok(mut bld_inv) = structure_inventories.get_mut(deposit.building_entity)
                    {
                        bld_inv.add(crate::items::ItemType::Document, 1);
                    }
                    if let Ok(mut docs) = structure_docs.get_mut(deposit.building_entity) {
                        docs.add(doc_title.to_string(), content.clone());
                    }
                    tracing::info!(
                        "[Deposit] {} deposited document '{}' into building ({} chars)",
                        name.0,
                        doc_title,
                        content.len()
                    );
                }
            }
            if !found {
                tracing::info!(
                    "[Deposit] {} doesn't have document '{}' — available docs listed in inventory",
                    name.0,
                    doc_title
                );
            }
        } else {
            // Regular item deposit at a regular building.
            let item_type = parse_item_name(&deposit.item_name);
            if let Some(item) = item_type {
                let allowed = structures
                    .get(deposit.building_entity)
                    .ok()
                    .and_then(|(_, restricted)| restricted)
                    .map_or(true, |restricted| restricted.allows(item));
                if !allowed {
                    tracing::warn!(
                        "[Deposit] {} tried to deposit disallowed item {}",
                        name.0,
                        deposit.item_name
                    );
                    commands.entity(entity).remove::<PendingDeposit>();
                    continue;
                }

                if item == crate::items::ItemType::Document {
                    let mut deposited = false;
                    if let Ok(mut docs) = agent_docs.get_mut(entity) {
                        if let Some((title, content)) = take_named_document(&mut docs) {
                            let document_item =
                                contained_items.get(entity).ok().and_then(|contained| {
                                    contained.items.iter().find_map(|item_entity| {
                                        item_lookup.get(*item_entity).ok().and_then(
                                            |(item_entity, kind, item_name, _)| {
                                                (kind.0 == crate::items::ItemType::Document
                                                    && item_name
                                                        .is_some_and(|name| name.0 == title))
                                                .then_some(item_entity)
                                            },
                                        )
                                    })
                                });
                            if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                                agent_inv.remove(crate::items::ItemType::Document, 1);
                            }
                            if let Some(item_entity) = document_item {
                                if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                    owner.0 = deposit.building_entity;
                                }
                                if let Ok(mut source) = contained_items.get_mut(entity) {
                                    source.remove(item_entity);
                                }
                                if let Ok(mut destination) =
                                    contained_items.get_mut(deposit.building_entity)
                                {
                                    destination.insert(item_entity);
                                }
                            }
                            if let Ok(mut bld_inv) =
                                structure_inventories.get_mut(deposit.building_entity)
                            {
                                bld_inv.add(crate::items::ItemType::Document, 1);
                            }
                            if let Ok(mut bld_docs) =
                                structure_docs.get_mut(deposit.building_entity)
                            {
                                bld_docs.add(title.clone(), content.clone());
                            }
                            tracing::info!(
                                "[Deposit] {} deposited document '{}' into building via generic document item ({} chars)",
                                name.0,
                                title,
                                content.len()
                            );
                            deposited = true;
                        }
                    }

                    if deposited {
                        commands.entity(entity).remove::<PendingDeposit>();
                        continue;
                    }
                }

                let mut success = false;
                if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                    if agent_inv.has(item, 1) {
                        agent_inv.remove(item, 1);
                        if let Ok(mut bld_inv) =
                            structure_inventories.get_mut(deposit.building_entity)
                        {
                            bld_inv.add(item, 1);
                            success = true;
                            tracing::info!(
                                "[Deposit] {} deposited {} into building",
                                name.0,
                                deposit.item_name
                            );
                        }
                    } else {
                        tracing::info!(
                            "[Deposit] {} doesn't have {} in inventory",
                            name.0,
                            deposit.item_name
                        );
                    }
                }
                if !success {
                    tracing::warn!(
                        "[Deposit] Failed: {} tried to deposit {} but doesn't have it",
                        name.0,
                        deposit.item_name
                    );
                }
            } else {
                tracing::warn!(
                    "[Deposit] Unknown item '{}'. Check your inventory for valid item names.",
                    deposit.item_name
                );
            }
        }

        commands.entity(entity).remove::<PendingDeposit>();
    }
}

/// Parse an item name string into an ItemType.
fn parse_item_name(name: &str) -> Option<crate::items::ItemType> {
    match name.to_lowercase().as_str() {
        "gold_egg" | "goldegg" | "golden_egg" => Some(crate::items::ItemType::GoldEgg),
        "gold_coin" | "goldcoin" => Some(crate::items::ItemType::GoldCoin),
        "coffee" => Some(crate::items::ItemType::Coffee),
        "muffin" => Some(crate::items::ItemType::Muffin),
        "sandwich" => Some(crate::items::ItemType::Sandwich),
        "rations" => Some(crate::items::ItemType::Rations),
        "soup" => Some(crate::items::ItemType::Soup),
        "coffee_beans" | "coffeebeans" => Some(crate::items::ItemType::CoffeeBeans),
        "flour" => Some(crate::items::ItemType::Flour),
        "raw_meat" | "rawmeat" => Some(crate::items::ItemType::RawMeat),
        "document" => Some(crate::items::ItemType::Document),
        "paycheck" => Some(crate::items::ItemType::Paycheck),
        "bounty_token" | "bountytoken" => Some(crate::items::ItemType::BountyToken),
        _ => None,
    }
}

fn take_named_document(docs: &mut crate::items::DocumentInventory) -> Option<(String, String)> {
    let mut titles: Vec<_> = docs.documents.keys().cloned().collect();
    titles.sort();
    let title = titles.into_iter().next()?;
    let content = docs.documents.remove(&title)?;
    Some((title, content))
}

fn preview_document(content: &str) -> String {
    const MAX_LEN: usize = 1200;
    if content.len() <= MAX_LEN {
        return content.to_string();
    }

    let mut clipped = content[..MAX_LEN].to_string();
    clipped.push_str("\n\n[truncated]");
    clipped
}

fn documents_root() -> std::path::PathBuf {
    std::env::var("DOCUMENTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("./documents"))
}

fn agent_documents_dir(agent_name: &str) -> std::path::PathBuf {
    documents_root().join(agent_name.to_lowercase())
}

/// System: process take_item — move item from building/tile to agent inventory.
pub fn process_take_items_system(
    mut commands: Commands,
    takes: Query<(Entity, &AgentName, &GridPos, &PendingTakeItem)>,
    mut agent_inventories: Query<&mut Inventory, With<AgentName>>,
    mut agent_docs: Query<&mut crate::items::DocumentInventory, With<AgentName>>,
    mut contained_items: Query<&mut crate::items::ContainedItems>,
    mut structure_inventories: Query<&mut Inventory, (With<StructureId>, Without<AgentName>)>,
    mut structure_docs: Query<
        &mut crate::items::DocumentInventory,
        (With<StructureId>, Without<AgentName>),
    >,
    item_lookup: Query<(
        Entity,
        &crate::items::ItemKind,
        Option<&crate::items::ItemName>,
    )>,
    mut item_containers: Query<&mut crate::items::ItemContainer>,
    mut tile_inventory: ResMut<crate::world::map::TileInventory>,
) {
    for (entity, name, pos, take) in &takes {
        // Check if it's a body (passed-out agent).
        if take.item_name.starts_with("body:") {
            // Pick up the body from the tile.
            if tile_inventory.take_item(pos.x, pos.y, &take.item_name) {
                // Add to the carrier's inventory as a special item.
                if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                    // Store as a generic "body" item — the name is tracked separately.
                    agent_inv
                        .items
                        .entry(crate::items::ItemType::Document)
                        .or_insert(0); // placeholder
                    tracing::info!(
                        "[Take] {} picked up {} from tile ({},{})",
                        name.0,
                        take.item_name,
                        pos.x,
                        pos.y
                    );
                }
            } else {
                tracing::info!(
                    "[Take] No {} at ({},{}) for {}",
                    take.item_name,
                    pos.x,
                    pos.y,
                    name.0
                );
            }
        } else {
            // Regular item — try building first, then tile.
            if take.item_name.starts_with("doc:") || take.item_name.ends_with(".md") {
                let doc_title = take
                    .item_name
                    .strip_prefix("doc:")
                    .unwrap_or(&take.item_name);
                let mut taken = false;
                let document_item =
                    contained_items
                        .get(take.building_entity)
                        .ok()
                        .and_then(|contained| {
                            contained.items.iter().find_map(|item_entity| {
                                item_lookup.get(*item_entity).ok().and_then(
                                    |(item_entity, kind, item_name)| {
                                        (kind.0 == crate::items::ItemType::Document
                                            && item_name.is_some_and(|name| name.0 == doc_title))
                                        .then_some(item_entity)
                                    },
                                )
                            })
                        });

                if let Ok(mut bld_docs) = structure_docs.get_mut(take.building_entity) {
                    if let Some(content) = bld_docs.documents.remove(doc_title) {
                        if let Ok(mut bld_inv) = structure_inventories.get_mut(take.building_entity)
                        {
                            bld_inv.remove(crate::items::ItemType::Document, 1);
                        }
                        if let Ok(mut docs) = agent_docs.get_mut(entity) {
                            docs.add(doc_title.to_string(), content.clone());
                        }
                        if let Some(item_entity) = document_item {
                            if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                owner.0 = entity;
                            }
                            if let Ok(mut source) = contained_items.get_mut(take.building_entity) {
                                source.remove(item_entity);
                            }
                            if let Ok(mut destination) = contained_items.get_mut(entity) {
                                destination.insert(item_entity);
                            }
                        }
                        if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                            agent_inv.add(crate::items::ItemType::Document, 1);
                        }
                        tracing::info!(
                            "[Take] {} took document '{}' from building ({} chars)",
                            name.0,
                            doc_title,
                            content.len()
                        );
                        taken = true;
                    }
                }

                if !taken {
                    tracing::info!(
                        "[Take] {} couldn't find document '{}' here",
                        name.0,
                        doc_title
                    );
                }

                commands.entity(entity).remove::<PendingTakeItem>();
                continue;
            }

            let item_type = parse_item_name(&take.item_name);
            let mut taken = false;

            if let Some(item) = item_type {
                if item == crate::items::ItemType::Document {
                    if let Ok(mut bld_docs) = structure_docs.get_mut(take.building_entity) {
                        if let Some((title, content)) = take_named_document(&mut bld_docs) {
                            let document_item = contained_items
                                .get(take.building_entity)
                                .ok()
                                .and_then(|contained| {
                                    contained.items.iter().find_map(|item_entity| {
                                        item_lookup.get(*item_entity).ok().and_then(
                                            |(item_entity, kind, item_name)| {
                                                (kind.0 == crate::items::ItemType::Document
                                                    && item_name
                                                        .is_some_and(|name| name.0 == title))
                                                .then_some(item_entity)
                                            },
                                        )
                                    })
                                });
                            if let Ok(mut bld_inv) =
                                structure_inventories.get_mut(take.building_entity)
                            {
                                bld_inv.remove(crate::items::ItemType::Document, 1);
                            }
                            if let Ok(mut docs) = agent_docs.get_mut(entity) {
                                docs.add(title.clone(), content.clone());
                            }
                            if let Some(item_entity) = document_item {
                                if let Ok(mut owner) = item_containers.get_mut(item_entity) {
                                    owner.0 = entity;
                                }
                                if let Ok(mut source) =
                                    contained_items.get_mut(take.building_entity)
                                {
                                    source.remove(item_entity);
                                }
                                if let Ok(mut destination) = contained_items.get_mut(entity) {
                                    destination.insert(item_entity);
                                }
                            }
                            if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                                agent_inv.add(crate::items::ItemType::Document, 1);
                            }
                            tracing::info!(
                                "[Take] {} took document '{}' from building via generic document item ({} chars)",
                                name.0,
                                title,
                                content.len()
                            );
                            taken = true;
                        }
                    }
                }

                // Try building inventory.
                if !taken {
                    if let Ok(mut bld_inv) = structure_inventories.get_mut(take.building_entity) {
                        if bld_inv.has(item, 1) {
                            bld_inv.remove(item, 1);
                            if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                                agent_inv.add(item, 1);
                                taken = true;
                                tracing::info!(
                                    "[Take] {} took {} from building",
                                    name.0,
                                    take.item_name
                                );
                            }
                        }
                    }
                }
                // Try tile inventory if building didn't have it.
                if !taken && tile_inventory.take_item(pos.x, pos.y, &take.item_name) {
                    if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                        agent_inv.add(item, 1);
                        taken = true;
                        tracing::info!(
                            "[Take] {} took {} from tile ({},{})",
                            name.0,
                            take.item_name,
                            pos.x,
                            pos.y
                        );
                    }
                }
            }
            if !taken {
                tracing::info!("[Take] {} couldn't find {} here", name.0, take.item_name);
            }
        }

        commands.entity(entity).remove::<PendingTakeItem>();
    }
}

/// Shared logic: upsert a single document into an agent's inventory + ECS items.
fn upsert_document(
    commands: &mut Commands,
    entity: Entity,
    title: &str,
    content: &str,
    agent_name: &str,
    inventories: &mut Query<&mut Inventory, With<AgentName>>,
    doc_inventories: &mut Query<&mut crate::items::DocumentInventory>,
    contained_items: &mut Query<&mut crate::items::ContainedItems, With<AgentName>>,
    item_lookup: &Query<(
        Entity,
        &crate::items::ItemKind,
        Option<&crate::items::ItemName>,
    )>,
) {
    let existing_item = contained_items.get(entity).ok().and_then(|contained| {
        contained.items.iter().find_map(|item_entity| {
            item_lookup
                .get(*item_entity)
                .ok()
                .and_then(|(item_entity, kind, item_name)| {
                    (kind.0 == crate::items::ItemType::Document
                        && item_name.is_some_and(|n| n.0 == title))
                    .then_some(item_entity)
                })
        })
    });

    if let Ok(mut docs) = doc_inventories.get_mut(entity) {
        docs.add(title.to_string(), content.to_string());
    }

    if let Some(item_entity) = existing_item {
        commands.entity(item_entity).insert((
            crate::items::ItemDescription(format!("Document '{}'", title)),
            crate::items::ItemContents(content.to_string()),
            crate::items::ItemContainer(entity),
        ));
    } else {
        if let Ok(mut inv) = inventories.get_mut(entity) {
            inv.add(crate::items::ItemType::Document, 1);
        }
        let item_entity = commands
            .spawn((
                crate::items::ItemKind(crate::items::ItemType::Document),
                crate::items::ItemName(title.to_string()),
                crate::items::ItemDescription(format!("Document '{}'", title)),
                crate::items::ItemContents(content.to_string()),
                crate::items::ItemContainer(entity),
            ))
            .id();
        if let Ok(mut contained) = contained_items.get_mut(entity) {
            contained.insert(item_entity);
        }
    }

    let agent_dir = agent_documents_dir(agent_name);
    let _ = std::fs::create_dir_all(&agent_dir);
    let _ = std::fs::write(agent_dir.join(title), content);

    tracing::info!(
        "[CreateDoc] {} created '{}' ({} chars)",
        agent_name,
        title,
        content.len()
    );
}

/// System: process create_document — agent writes a document to their inventory.
pub fn process_create_documents_system(
    mut commands: Commands,
    pending: Query<(Entity, &AgentName, &AgentId, &PendingCreateDocument)>,
    pending_batch: Query<(Entity, &AgentName, &PendingCreateDocumentBatch)>,
    mut inventories: Query<&mut Inventory, With<AgentName>>,
    mut doc_inventories: Query<&mut crate::items::DocumentInventory>,
    mut contained_items: Query<&mut crate::items::ContainedItems, With<AgentName>>,
    item_lookup: Query<(
        Entity,
        &crate::items::ItemKind,
        Option<&crate::items::ItemName>,
    )>,
) {
    for (entity, name, _agent_id, doc) in &pending {
        upsert_document(
            &mut commands,
            entity,
            &doc.title,
            &doc.content,
            &name.0,
            &mut inventories,
            &mut doc_inventories,
            &mut contained_items,
            &item_lookup,
        );
        commands.entity(entity).remove::<PendingCreateDocument>();
    }

    // Process batch returns (e.g. from bounty rejection with multiple docs).
    for (entity, name, batch) in &pending_batch {
        for (title, content) in &batch.docs {
            upsert_document(
                &mut commands,
                entity,
                title,
                content,
                &name.0,
                &mut inventories,
                &mut doc_inventories,
                &mut contained_items,
                &item_lookup,
            );
        }
        commands.entity(entity).remove::<PendingCreateDocumentBatch>();
    }
}

/// System: auto-exchange business cards when agents are in a conversation.
/// Each agent adds their partner as a contact if not already known.
pub fn auto_exchange_cards_system(
    mut agents: Query<(
        &AgentName,
        &AgentId,
        &ActiveConversation,
        &mut BusinessCards,
    )>,
) {
    // Collect partner IDs first to avoid borrow conflicts.
    let partner_ids: std::collections::HashMap<Entity, (String, String)> = agents
        .iter()
        .map(|(name, id, _convo, _)| (_convo.partner, (name.0.clone(), id.0.to_string())))
        .collect();

    for (_name, _id, convo, mut cards) in &mut agents {
        if !cards.contacts.contains_key(&convo.partner_name) && cards.cards_remaining > 0 {
            // Look up partner's ID from the pre-collected map.
            let partner_id = partner_ids
                .get(&convo.partner)
                .map(|(_, id)| id.clone())
                .unwrap_or_default();
            cards
                .contacts
                .insert(convo.partner_name.clone(), partner_id);
            cards.cards_remaining -= 1;
            tracing::info!(
                "[Cards] {} received {}'s business card",
                _name.0,
                convo.partner_name
            );
        }
    }
}

/// System: apply pending MCP actions to agent entities.
pub fn apply_mcp_actions_system(
    mut commands: Commands,
    mut pending: ResMut<PendingActions>,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    mut event_log: ResMut<AgentEventLog>,
    mut suggestion_box: ResMut<SuggestionBox>,
    mut boards_mcp: Query<(&mut BountyTokenStore, &mut BountyDropbox), With<BountyBoard>>,
    sessions: Res<AgentSessions>,
    mut item_params: ParamSet<(
        Query<&mut crate::items::ContainedItems>,
        Query<&mut crate::items::DocumentInventory, With<AgentName>>,
        Query<&mut crate::items::DocumentInventory, (With<StructureId>, Without<AgentName>)>,
        Query<(
            Entity,
            &crate::items::ItemKind,
            Option<&crate::items::ItemName>,
            Option<&crate::items::BountyTokenInfo>,
        )>,
        Query<&mut crate::items::ItemContainer>,
    )>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &mut AgentGoal,
        &mut ThoughtBubble,
        &KnownLocations,
        Option<&ShiftWorker>,
        &mut Needs,
        Option<&ActiveConversation>,
        &BusinessCards,
    )>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
    mut trade_proposals: Query<(Entity, &mut TradeProposal)>,
    library: Res<crate::world::bounty::Library>,
    mut agent_inventories_mcp: Query<&mut Inventory, With<AgentName>>,
    mut agent_context_windows: Query<&mut crate::agents::token_tracking::ContextWindow, With<AgentName>>,
) {
    let Some((mut bounty_registry, mut dropbox)) = boards_mcp.iter_mut().next() else {
        return;
    };

    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter()
        .map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone()))
        .collect();

    // Pre-collect agent info for cross-agent lookups (avoids borrow conflicts).
    let agent_snapshot: Vec<(Entity, String, GridPos, bool)> = agents
        .iter()
        .map(|(e, n, p, _, _, _, _, _, ac, _)| (e, n.0.clone(), *p, ac.is_some()))
        .collect();

    for mcp_action in pending.actions.drain(..) {
        // Find the agent entity by name.
        let agent = agents
            .iter_mut()
            .find(|(_, name, _, _, _, _, _, _, _, _)| name.0 == mcp_action.agent_name);

        let Some((
            entity,
            name,
            pos,
            mut goal,
            mut thought,
            known_locs,
            shift_worker,
            mut needs,
            active_convo,
            business_cards,
        )) = agent
        else {
            tracing::warn!("[MCP] Agent '{}' not found", mcp_action.agent_name);
            continue;
        };

        let action_str = &mcp_action.action;
        tracing::info!(
            "[MCP:{}] action={} building={:?} service={:?}",
            name.0,
            action_str,
            mcp_action.building,
            mcp_action.service
        );

        event_log.push(LogEvent {
            tick: tick.0,
            agent: name.0.clone(),
            kind: LogKind::Action,
            text: format!(
                "→ {} {:?} {:?}",
                action_str, mcp_action.building, mcp_action.service
            ),
        });

        // Helper: find building and get location info for error messages.
        let find_building = |name: &str| -> Option<(Entity, GridPos, String)> {
            structure_list
                .iter()
                .find(|(_, _, s)| s == name)
                .map(|(e, p, s)| (*e, *p, s.clone()))
        };

        let known_buildings: Vec<String> =
            structure_list.iter().map(|(_, _, s)| s.clone()).collect();

        match action_str.as_str() {
            "go_to_board" => {
                // Preserve ReturningToBoard and ExecutingBounty goals.
                if !matches!(*goal, AgentGoal::ReturningToBoard(_) | AgentGoal::ExecutingBounty(_)) {
                    *goal = AgentGoal::GoingToBoard;
                }
                if let Some(board) = known_locs
                    .locations
                    .values()
                    .find(|l| l.name == "bounty_board")
                {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        if matches!(*goal, AgentGoal::ReturningToBoard(_)) {
                            thought.0 = format!(
                                "Returning to board to collect bounty reward ({} tiles).",
                                tiles
                            );
                        } else {
                            thought.0 = format!("Heading to the bounty board ({} tiles).", tiles);
                        }
                    } else {
                        thought.0 = "ERROR: Can't find path to the bounty board!".into();
                    }
                } else {
                    thought.0 =
                        "ERROR: Don't know where the bounty board is. Try look_around first."
                            .into();
                }
            }

            "go_to_service" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");
                let service = mcp_action.service.as_deref().unwrap_or("browse");

                if let Some((bld_entity, entrance, _)) = find_building(building) {
                    // Preserve ExecutingBounty goal — walking to a service IS part of executing.
                    if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                        *goal = AgentGoal::GoingToService {
                            building: bld_entity,
                            service: service.into(),
                        };
                    }
                    if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        thought.0 =
                            format!("Walking to {} for {} ({} tiles).", building, service, tiles);
                    } else {
                        thought.0 =
                            format!("ERROR: Can't find path to {}. Try look_around.", building);
                    }
                } else {
                    thought.0 = format!(
                        "ERROR: Unknown building '{}'. Known buildings: {}. Try look_around to discover more.",
                        building, known_buildings.join(", "),
                    );
                }
            }

            "look_around" => {
                commands.entity(entity).insert(WantsToLook);
                thought.0 = "Scanning surroundings...".into();
            }

            "wander" => {
                let walkable = map.walkable_positions();
                let idx = (entity.to_bits() as usize + tick.0 as usize) % walkable.len();
                if let Some(p) = pathfinding::bfs(&map, *pos, walkable[idx]) {
                    let tiles = p.len();
                    if tiles > 0 {
                        commands.entity(entity).insert(Path(p));
                        if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                            *goal = AgentGoal::Wandering;
                        }
                        thought.0 = format!("Wandering ({} tiles).", tiles);
                    }
                }
            }

            "work_shift" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");

                if let Some((bld_entity, entrance, _)) = find_building(building) {
                    // Check if agent is already at the building.
                    let at_building = pos.x == entrance.x && pos.y == entrance.y;
                    if at_building {
                        // Start shift immediately. Preserve ExecutingBounty goal.
                        if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                            *goal = AgentGoal::GoingToService {
                                building: bld_entity,
                                service: "work_shift".into(),
                            };
                        }
                        thought.0 = format!("Starting shift at {}.", building);
                    } else {
                        // Walk there first. Preserve ExecutingBounty goal.
                        if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                            *goal = AgentGoal::GoingToService {
                                building: bld_entity,
                                service: "work_shift".into(),
                            };
                        }
                        if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                            let tiles = p.len();
                            commands.entity(entity).insert(Path(p));
                            thought.0 = format!(
                                "Walking to {} to work a shift ({} tiles). You must be at the building to start working.",
                                building, tiles,
                            );
                        } else {
                            thought.0 = format!("ERROR: Can't find path to {}.", building);
                        }
                    }
                } else {
                    thought.0 = format!(
                        "ERROR: Unknown building '{}'. Shiftable buildings: cafe, market, warehouse, hotel. Use go_to_service to walk there first.",
                        building,
                    );
                }
            }

            "leave_shift" => {
                // Allow leaving a shift whether the goal is WorkingShift or ExecutingBounty
                // (the agent may be working a shift as part of a bounty).
                let on_shift = shift_worker.is_some();
                if on_shift {
                    // Preserve ExecutingBounty goal — shift was part of bounty work.
                    if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                        *goal = AgentGoal::Idle;
                    }
                    thought.0 = "Left the shift. Paycheck earned based on ticks worked.".into();
                } else {
                    thought.0 = "ERROR: Not currently working a shift. Nothing to leave.".into();
                }
            }

            "complete_bounty" => {
                // Must be at the bounty board to submit completion.
                let at_board = known_locs
                    .locations
                    .values()
                    .find(|l| l.name == "bounty_board")
                    .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);

                if !at_board {
                    thought.0 = "You must be at the bounty board to submit a bounty for completion. Use go_to_board first.".into();
                } else {
                    // Check if agent has deposited a bounty token in the dropbox.
                    let dropbox_slot = dropbox.get_slot(entity);
                    let dropbox_bounty_id = dropbox_slot.and_then(|s| s.bounty_token_id);

                    if let Some(bounty_id) = dropbox_bounty_id {
                        // Token is in the dropbox — submit for GM verification.
                        *goal = AgentGoal::ReturningToBoard(bounty_id);
                        thought.0 = "Bounty submitted for Game Master review! Your token and proof items are in the dropbox. Waiting for verdict...".into();
                    } else {
                        // No token in dropbox — check if they have an active bounty.
                        let has_active = match *goal {
                            AgentGoal::ExecutingBounty(_) | AgentGoal::ReturningToBoard(_) => true,
                            _ => bounty_registry.tokens.values().any(|b| {
                                b.claimed_by == Some(entity)
                                    && b.state == crate::world::bounty::BountyState::Claimed
                            }),
                        };
                        if has_active {
                            thought.0 = "ERROR: You must deposit your bounty_token at the board first! Use deposit_item with service='bounty_token', then deposit your proof documents/items, THEN call complete_bounty.".into();
                        } else {
                            thought.0 = "ERROR: No active bounty to complete. Claim one at the bounty board first.".into();
                        }
                    }
                }
            }

            "go_to" => {
                if let (Some(x), Some(y)) = (mcp_action.x, mcp_action.y) {
                    // GPT-series fills optional integer params with 0 instead of omitting.
                    // Treat x=0 AND y=0 as "no coordinates" unless agent is already near (0,0).
                    let near_origin = pos.x.abs() <= 2 && pos.y.abs() <= 2;
                    if x == 0 && y == 0 && !near_origin {
                        thought.0 = "ERROR: go_to requires explicit x and y coordinates. If you want to visit a building, use go_to_service instead. Map is 40x200 (x: 0-39, y: 0-199).".into();
                    } else {
                        let target = GridPos { x, y };
                        if !map.is_walkable(&target) {
                            thought.0 = format!("ERROR: ({},{}) is not walkable (might be inside a building). Try nearby coordinates.", x, y);
                        } else if let Some(p) = pathfinding::bfs(&map, *pos, target) {
                            let tiles = p.len();
                            commands.entity(entity).insert(Path(p));
                            // Preserve ExecutingBounty goal — don't lose the active bounty context.
                            if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                                *goal = AgentGoal::Wandering;
                            }
                            thought.0 = format!("Walking to ({},{}) — {} tiles.", x, y, tiles);
                        } else {
                            thought.0 = format!(
                                "ERROR: Can't find path to ({},{}). The map is 40x200.",
                                x, y
                            );
                        }
                    }
                } else {
                    thought.0 =
                        "ERROR: go_to requires x and y coordinates. Map is 40x200 (x: 0-39, y: 0-199).".into();
                }
            }

            "chat_with" => {
                let target = mcp_action.agent_target.as_deref().unwrap_or("someone");
                // Preserve ExecutingBounty goal — chatting may be part of bounty work.
                if !matches!(*goal, AgentGoal::ExecutingBounty(_)) {
                    *goal = AgentGoal::Wandering;
                }
                thought.0 = format!("Looking to chat with {}...", target);
            }

            "send_message" => {
                if let (Some(recipient), Some(text)) = (&mcp_action.agent_target, &mcp_action.text)
                {
                    // Must have recipient's business card to send messages.
                    if business_cards.contacts.contains_key(recipient.as_str()) {
                        commands.entity(entity).insert(WantsToSendMessage {
                            recipient_name: recipient.clone(),
                            text: text.clone(),
                        });
                        thought.0 = format!("Sent message to {}.", recipient);
                    } else {
                        thought.0 = format!("ERROR: You don't have {}'s business card. Start a face-to-face conversation first to exchange cards.", recipient);
                    }
                } else {
                    thought.0 = "ERROR: send_message requires 'agent' (name from your business cards) and 'text' parameters.".into();
                }
            }

            "claim_bounty" => {
                // Allow claiming if at the board entrance.
                let at_board = known_locs
                    .locations
                    .values()
                    .find(|l| l.name == "bounty_board")
                    .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);
                if !at_board && !matches!(*goal, AgentGoal::InteractingWithBoard | AgentGoal::ExecutingBounty(_) | AgentGoal::GoingToBoard | AgentGoal::ReturningToBoard(_)) {
                    thought.0 =
                        "Must be at the bounty board to claim a bounty! Go there first.".into();
                } else if bounty_registry.tokens.values().any(|b| {
                    b.claimed_by == Some(entity)
                        && b.state == crate::world::bounty::BountyState::Claimed
                }) {
                    thought.0 = "ERROR: You already have an active bounty. Complete or cancel it before claiming another. You can only hold one bounty token at a time.".into();
                } else {
                    let keyword = mcp_action
                        .text
                        .as_deref()
                        .or(mcp_action.service.as_deref())
                        .unwrap_or("");

                    // Find available bounty by ID prefix or keyword match.
                    let bounty_match = bounty_registry
                        .available()
                        .iter()
                        .find(|b| {
                            if keyword.is_empty() {
                                return true;
                            }
                            // Match by short ID (first 6 chars of UUID).
                            let short_id = &b.id.to_string()[..6];
                            if keyword == short_id || keyword == b.id.to_string() {
                                return true;
                            }
                            b.description
                                .to_lowercase()
                                .contains(&keyword.to_lowercase())
                        })
                        .map(|b| b.id);

                    if let Some(bounty_id) = bounty_match {
                        if let Some(bounty) = bounty_registry.claim(bounty_id, entity, tick.0) {
                            let desc = bounty.description.clone();
                            let claim_items = bounty.claim_items.clone();

                            // Give bounty token + any claim items.
                            let mut items_to_give = claim_items;
                            items_to_give.push((crate::items::ItemType::BountyToken, 1));
                            commands.entity(entity).insert(PendingClaimItems {
                                items: items_to_give,
                            });

                            let instructions = bounty
                                .hidden_criteria
                                .split("\n\nGM:")
                                .next()
                                .unwrap_or("")
                                .strip_prefix("Instructions for agent: ")
                                .unwrap_or("Complete the task and return to the board.")
                                .to_string();
                            let token_entity = commands
                                .spawn((
                                    crate::items::ItemKind(crate::items::ItemType::BountyToken),
                                    crate::items::ItemName(format!("bounty: {}", desc)),
                                    crate::items::ItemDescription(instructions.clone()),
                                    crate::items::ItemContainer(entity),
                                    crate::items::BountyTokenInfo {
                                        bounty_id: bounty_id.to_string(),
                                        title: desc.clone(),
                                        reward: bounty.reward_gold,
                                        instructions: instructions.clone(),
                                    },
                                ))
                                .id();
                            if let Ok(mut contained) = item_params.p0().get_mut(entity) {
                                contained.insert(token_entity);
                            }

                            thought.0 =
                                format!("Claimed: {}. INSTRUCTIONS: {}", desc, instructions);

                            event_log.push(LogEvent {
                                tick: tick.0,
                                agent: name.0.clone(),
                                kind: LogKind::Action,
                                text: format!("CLAIMED: {} ({}g)", desc, bounty.reward_gold),
                            });

                            tracing::info!("[MCP:{}] claimed bounty: {}", name.0, desc);

                            // Leave the board queue.
                            *goal = AgentGoal::ExecutingBounty(bounty_id);
                        } else {
                            thought.0 =
                                "Couldn't claim that bounty — it may already be taken.".into();
                        }
                    } else {
                        let avail = bounty_registry.available().len();
                        if avail == 0 {
                            thought.0 = "No bounties available right now. Check back later or work a shift to earn gold while you wait.".into();
                        } else {
                            thought.0 = format!("No bounty matching '{}'. {} bounties available — try claim_bounty without a keyword to grab the first one.", keyword, avail);
                        }
                    }
                }
            }

            "leave_board" => {
                if let Some(slot) = dropbox.clear_slot(entity) {
                    let bounty = slot
                        .bounty_token_id
                        .and_then(|bounty_id| bounty_registry.get(bounty_id));
                    if let Some((board_entity, _, _)) = structure_list
                        .iter()
                        .find(|(_, _, name)| name == "bounty_board")
                    {
                        if let Ok(mut board_items) = item_params.p0().get_mut(*board_entity) {
                            if let Some(item_entity) = slot.bounty_token_item {
                                board_items.remove(item_entity);
                            }
                            for item_entity in &slot.document_items {
                                board_items.remove(*item_entity);
                            }
                        }
                    }
                    if let Ok(mut contained) = item_params.p0().get_mut(entity) {
                        queue_dropbox_return(&mut commands, entity, &mut contained, slot, bounty);
                    }
                    thought.0 = "Left the bounty board. Your temporary dropbox contents were returned to your inventory.".into();
                } else {
                    thought.0 = "Left the bounty board.".into();
                }
                *goal = AgentGoal::Idle;
            }

            "start_conversation" => {
                let target_name = mcp_action.agent_target.as_deref().unwrap_or("unknown");

                if active_convo.is_some() {
                    thought.0 =
                        "ERROR: Already in a conversation. Use end_conversation first.".into();
                } else {
                    // Look up target from pre-collected snapshot (avoids borrow conflict).
                    let target_info = agent_snapshot
                        .iter()
                        .find(|(_, n, _, _)| n == target_name)
                        .map(|(e, n, p, in_convo)| (*e, n.clone(), *p, *in_convo));

                    match target_info {
                        None => {
                            thought.0 = format!("ERROR: Agent '{}' not found.", target_name);
                        }
                        Some((_, _, _, true)) => {
                            thought.0 =
                                format!("ERROR: {} is already in a conversation.", target_name);
                        }
                        Some((target_entity, partner_name, target_pos, false)) => {
                            let dist = (pos.x - target_pos.x).abs() + (pos.y - target_pos.y).abs();
                            if dist > 2 {
                                thought.0 = format!(
                                    "ERROR: {} is {} tiles away — must be within 2 tiles to start a conversation.",
                                    target_name, dist,
                                );
                            } else {
                                // Stop both agents from walking.
                                commands.entity(entity).remove::<Path>();
                                commands.entity(target_entity).remove::<Path>();

                                // Insert ActiveConversation and ConversationLog on both.
                                let self_name = name.0.clone();

                                commands.entity(entity).insert((
                                    ActiveConversation {
                                        partner: target_entity,
                                        partner_name: partner_name.clone(),
                                        started_tick: tick.0,
                                    },
                                    ConversationLog::default(),
                                ));

                                commands.entity(target_entity).insert((
                                    ActiveConversation {
                                        partner: entity,
                                        partner_name: self_name.clone(),
                                        started_tick: tick.0,
                                    },
                                    ConversationLog::default(),
                                ));

                                thought.0 = format!("Started conversation with {}.", partner_name);

                                event_log.push(LogEvent {
                                    tick: tick.0,
                                    agent: self_name.clone(),
                                    kind: LogKind::System,
                                    text: format!("Started conversation with {}", partner_name),
                                });

                                // Notify the target agent via their session.
                                if let Some(session) = sessions.sessions.get(&target_entity) {
                                    let _ = session.prompt_tx.try_send(
                                        format!("{} started a face-to-face conversation with you! Use 'say' to respond or 'end_conversation' to leave.", self_name),
                                    );
                                }
                            }
                        }
                    }
                }
            }

            "say" => {
                let message_text = mcp_action.text.as_deref().unwrap_or("");

                if message_text.is_empty() {
                    thought.0 = "ERROR: 'say' requires a 'text' parameter.".into();
                } else if let Some(convo) = active_convo {
                    let partner_entity = convo.partner;
                    let partner_name = convo.partner_name.clone();
                    let self_name = name.0.clone();

                    let msg = ConversationMessage {
                        speaker: self_name.clone(),
                        text: message_text.to_string(),
                        tick: tick.0,
                    };

                    // Add message to both agents' ConversationLogs via commands.
                    // We can't mutably access ConversationLog from the current query,
                    // so we use a deferred approach: insert updated logs.
                    // For simplicity, we'll use commands to add a marker component
                    // and handle it. But since ConversationLog isn't in the query,
                    // let's just use commands to run a closure.

                    // Actually, we can't access ConversationLog here since it's not
                    // in the query. We'll queue a pending conversation message instead.
                    // For now, let's insert the message on the entity directly.
                    // We'll handle this with a separate small system or direct commands.

                    // Use a simpler approach: store pending messages in a resource.
                    // But to keep it self-contained, we'll just rebuild the log component.

                    thought.0 = format!("Said: \"{}\"", message_text);

                    // Boost boredom for the speaker.
                    needs.boredom = (needs.boredom + 5.0).min(100.0);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: self_name.clone(),
                        kind: LogKind::Speech,
                        text: format!("[to {}] {}", partner_name, message_text),
                    });

                    // Notify the partner via their session.
                    if let Some(session) = sessions.sessions.get(&partner_entity) {
                        let _ = session
                            .prompt_tx
                            .try_send(format!("{} says: \"{}\"", self_name, message_text));
                    }

                    // Boost partner's boredom too (via deferred command).
                    // We can't get mut Needs for the partner from the same query,
                    // so we'll queue a pending boredom boost.
                    // Store the message and partner boredom boost as pending.
                    // We'll use a simple approach: insert a marker component.
                    commands.entity(entity).insert(PendingConversationMessage {
                        message: msg.clone(),
                        partner: partner_entity,
                    });
                } else {
                    thought.0 =
                        "ERROR: Not in a conversation. Use start_conversation first.".into();
                }
            }

            "end_conversation" => {
                if let Some(convo) = active_convo {
                    let partner_entity = convo.partner;
                    let partner_name = convo.partner_name.clone();
                    let self_name = name.0.clone();

                    // Remove conversation components from both agents.
                    commands.entity(entity).remove::<ActiveConversation>();
                    commands.entity(entity).remove::<ConversationLog>();
                    commands
                        .entity(partner_entity)
                        .remove::<ActiveConversation>();
                    commands.entity(partner_entity).remove::<ConversationLog>();

                    thought.0 = format!("Ended conversation with {}.", partner_name);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: self_name.clone(),
                        kind: LogKind::System,
                        text: format!("Ended conversation with {}", partner_name),
                    });

                    // Notify the partner.
                    if let Some(session) = sessions.sessions.get(&partner_entity) {
                        let _ = session
                            .prompt_tx
                            .try_send(format!("{} ended the conversation.", self_name));
                    }
                } else {
                    thought.0 = "ERROR: Not in a conversation. Nothing to end.".into();
                }
            }

            "offer_trade" => {
                if let Some(convo) = active_convo {
                    let partner_entity = convo.partner;
                    let partner_name = convo.partner_name.clone();
                    let self_name = name.0.clone();

                    // Parse offered items from 'text' and requested items from 'service'.
                    let offered_str = mcp_action.text.as_deref().unwrap_or("");
                    let requested_str = mcp_action.service.as_deref().unwrap_or("");

                    let offered = trading::parse_item_list(offered_str);
                    let requested = trading::parse_item_list(requested_str);

                    match (offered, requested) {
                        (Ok(offered_items), Ok(requested_items))
                            if !offered_items.is_empty() || !requested_items.is_empty() =>
                        {
                            // Check for existing trade proposal between these agents.
                            let existing = trade_proposals.iter().any(|(_, tp)| {
                                (tp.proposer == entity && tp.responder == partner_entity)
                                    || (tp.proposer == partner_entity && tp.responder == entity)
                            });

                            if existing {
                                thought.0 = "ERROR: A trade is already pending with this partner. Accept, reject, or wait.".into();
                            } else {
                                let offered_desc: Vec<String> =
                                    offered_items.iter().map(|i| i.to_string()).collect();
                                let requested_desc: Vec<String> =
                                    requested_items.iter().map(|i| i.to_string()).collect();

                                // Spawn a TradeProposal entity.
                                commands.spawn(TradeProposal {
                                    proposer: entity,
                                    responder: partner_entity,
                                    offered_items,
                                    requested_items,
                                    proposer_accepted: true, // proposer auto-accepts their own offer
                                    responder_accepted: false,
                                });

                                thought.0 = format!(
                                    "Offered trade to {}: giving [{}] for [{}].",
                                    partner_name,
                                    offered_desc.join(", "),
                                    requested_desc.join(", "),
                                );

                                event_log.push(LogEvent {
                                    tick: tick.0,
                                    agent: self_name.clone(),
                                    kind: LogKind::Action,
                                    text: format!(
                                        "Offered trade to {}: [{}] for [{}]",
                                        partner_name,
                                        offered_desc.join(", "),
                                        requested_desc.join(", "),
                                    ),
                                });

                                // Notify the partner.
                                if let Some(session) = sessions.sessions.get(&partner_entity) {
                                    let _ = session.prompt_tx.try_send(
                                        format!(
                                            "{} offers a trade: they give [{}] and want [{}]. Use accept_trade or reject_trade to respond.",
                                            self_name,
                                            offered_desc.join(", "),
                                            requested_desc.join(", "),
                                        ),
                                    );
                                }
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            thought.0 = format!("ERROR: {}", e);
                        }
                        _ => {
                            thought.0 = "ERROR: offer_trade requires 'text' (offered items, comma-separated) and 'service' (requested items, comma-separated).".into();
                        }
                    }
                } else {
                    thought.0 = "ERROR: Must be in a conversation to offer a trade. Use start_conversation first.".into();
                }
            }

            "accept_trade" => {
                let self_name = name.0.clone();

                // Find a trade proposal where this agent is a participant.
                let proposal = trade_proposals
                    .iter_mut()
                    .find(|(_, tp)| tp.proposer == entity || tp.responder == entity);

                if let Some((_, mut tp)) = proposal {
                    if tp.proposer == entity {
                        tp.proposer_accepted = true;
                        thought.0 = "Accepted the trade on your side.".into();
                    } else {
                        tp.responder_accepted = true;
                        thought.0 = "Accepted the trade on your side.".into();
                    }

                    let partner = if tp.proposer == entity {
                        tp.responder
                    } else {
                        tp.proposer
                    };

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: self_name.clone(),
                        kind: LogKind::Action,
                        text: "Accepted trade.".into(),
                    });

                    // Notify the partner.
                    if let Some(session) = sessions.sessions.get(&partner) {
                        let _ = session
                            .prompt_tx
                            .try_send(format!("{} accepted the trade!", self_name));
                    }
                } else {
                    thought.0 = "ERROR: No pending trade to accept.".into();
                }
            }

            "reject_trade" => {
                let self_name = name.0.clone();

                // Find and remove a trade proposal where this agent is a participant.
                let proposal = trade_proposals
                    .iter()
                    .find(|(_, tp)| tp.proposer == entity || tp.responder == entity)
                    .map(|(e, tp)| (e, tp.proposer, tp.responder));

                if let Some((proposal_entity, proposer, responder)) = proposal {
                    let partner = if proposer == entity {
                        responder
                    } else {
                        proposer
                    };

                    commands.entity(proposal_entity).despawn();

                    thought.0 = "Rejected the trade.".into();

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: self_name.clone(),
                        kind: LogKind::Action,
                        text: "Rejected trade.".into(),
                    });

                    // Notify the partner.
                    if let Some(session) = sessions.sessions.get(&partner) {
                        let _ = session
                            .prompt_tx
                            .try_send(format!("{} rejected the trade.", self_name));
                    }
                } else {
                    thought.0 = "ERROR: No pending trade to reject.".into();
                }
            }

            "deposit_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("unknown");

                // Must be inside a building.
                let building = structure_list
                    .iter()
                    .find(|(_, entrance, name)| pos.x == entrance.x && pos.y == entrance.y);

                if let Some((bld_entity, _, bld_name)) = building {
                    commands.entity(entity).insert(PendingDeposit {
                        item_name: item_name.to_string(),
                        building_entity: *bld_entity,
                    });
                    thought.0 = format!("Deposited {} at {} (your position: ({},{})). If this is the WRONG building, you need to go_to_service to the correct one first!", item_name, bld_name, pos.x, pos.y);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: name.0.clone(),
                        kind: LogKind::Action,
                        text: format!("DEPOSIT: {} → {}", item_name, bld_name),
                    });
                } else {
                    thought.0 = format!("ERROR: You are not at any building entrance (your position: ({},{})). Walk to the building first using go_to_service, wait to arrive, then deposit.", pos.x, pos.y);
                }
            }

            "take_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("unknown");

                let building = structure_list
                    .iter()
                    .find(|(_, entrance, _)| pos.x == entrance.x && pos.y == entrance.y);

                if let Some((bld_entity, _, bld_name)) = building {
                    commands.entity(entity).insert(PendingTakeItem {
                        item_name: item_name.to_string(),
                        building_entity: *bld_entity,
                    });
                    thought.0 = format!("Taking {} from {} (position: ({},{}))", item_name, bld_name, pos.x, pos.y);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: name.0.clone(),
                        kind: LogKind::Action,
                        text: format!("TAKE: {} ← {}", item_name, bld_name),
                    });
                } else {
                    thought.0 = "ERROR: Must be at a building entrance to take items.".into();
                }
            }

            "create_document" => {
                let title = mcp_action
                    .service
                    .as_deref()
                    .unwrap_or("untitled.md")
                    .to_string();
                let content = mcp_action.text.as_deref().unwrap_or("").to_string();

                if content.is_empty() {
                    thought.0 =
                        "ERROR: create_document requires 'text' with the markdown content.".into();
                } else {
                    commands.entity(entity).insert(PendingCreateDocument {
                        title: title.clone(),
                        content: content.clone(),
                    });
                    thought.0 = format!("Created document '{}'.", title);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: name.0.clone(),
                        kind: LogKind::Action,
                        text: format!("CREATED DOC: {}", title),
                    });
                }
            }

            "append_document" => {
                let doc_name = mcp_action.service.as_deref().unwrap_or("document");
                let append_text = mcp_action.text.as_deref().unwrap_or("").trim();

                if append_text.is_empty() {
                    thought.0 = "ERROR: append_document requires 'service' with a document title and 'text' with the addendum.".into();
                } else if let Ok(mut docs) = item_params.p1().get_mut(entity) {
                    let target_title = if doc_name == "document" {
                        let mut titles: Vec<_> = docs.documents.keys().cloned().collect();
                        titles.sort();
                        titles.into_iter().next()
                    } else {
                        Some(
                            doc_name
                                .strip_prefix("doc:")
                                .unwrap_or(doc_name)
                                .to_string(),
                        )
                    };

                    let Some(target_title) = target_title else {
                        thought.0 =
                            "ERROR: You are not carrying any documents to append to.".into();
                        continue;
                    };

                    if let Some(existing) = docs.documents.get_mut(&target_title) {
                        existing.push_str("\n\n## Addendum\n");
                        existing.push_str(append_text);
                        let updated_contents = existing.clone();
                        thought.0 = format!("Appended an addendum to '{}'.", target_title);

                        let agent_dir = agent_documents_dir(&name.0);
                        let _ = std::fs::create_dir_all(&agent_dir);
                        let _ = std::fs::write(agent_dir.join(&target_title), &updated_contents);

                        event_log.push(LogEvent {
                            tick: tick.0,
                            agent: name.0.clone(),
                            kind: LogKind::Action,
                            text: format!("APPENDED DOC: {}", target_title),
                        });

                        let contained_item_ids = item_params
                            .p0()
                            .get_mut(entity)
                            .map(|contained| contained.items.clone())
                            .unwrap_or_default();
                        for item_entity in contained_item_ids {
                            if let Ok((item_entity, kind, item_name, _)) =
                                item_params.p3().get(item_entity)
                            {
                                if kind.0 == crate::items::ItemType::Document
                                    && item_name.is_some_and(|name| name.0 == target_title)
                                {
                                    commands.entity(item_entity).insert(
                                        crate::items::ItemContents(updated_contents.clone()),
                                    );
                                }
                            }
                        }
                    } else {
                        thought.0 = format!(
                            "ERROR: You are not carrying '{}'. Take the note first, then append to it.",
                            target_title
                        );
                    }
                }
            }

            "inspect_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("unknown");

                if item_name == "bounty_token" || item_name.starts_with("bounty") {
                    // Show bounty token details via session message.
                    if let Some(session) = sessions.sessions.get(&entity) {
                        let contained_item_ids = item_params
                            .p0()
                            .get_mut(entity)
                            .map(|contained| contained.items.clone())
                            .unwrap_or_default();
                        let token_info = contained_item_ids.into_iter().find_map(|item_entity| {
                            item_params.p3().get(item_entity).ok().and_then(
                                |(_, kind, _item_name, token_info)| {
                                    (kind.0 == crate::items::ItemType::BountyToken)
                                        .then(|| token_info.cloned())
                                        .flatten()
                                },
                            )
                        });
                        if let Some(info) = token_info {
                            let _ = session.prompt_tx.try_send(format!(
                                "=== BOUNTY TOKEN ===\nTitle: {}\nReward: {}g\nID: {}\nInstructions: {}\n=== END TOKEN ===",
                                info.title,
                                info.reward,
                                &info.bounty_id[..6.min(info.bounty_id.len())],
                                info.instructions,
                            ));
                            thought.0 = format!("Reading bounty token: {}", info.title);
                        } else {
                            thought.0 = "You don't have a bounty token.".into();
                        }
                    }
                } else if item_name.starts_with("doc:") || item_name.ends_with(".md") {
                    let doc_title = item_name.strip_prefix("doc:").unwrap_or(item_name);
                    let mut found_content = None;
                    let mut found_location = "inventory".to_string();

                    if let Ok(docs) = item_params.p1().get_mut(entity) {
                        found_content = docs.documents.get(doc_title).cloned();
                    }

                    if found_content.is_none() {
                        let building = structure_list
                            .iter()
                            .find(|(_, entrance, _)| pos.x == entrance.x && pos.y == entrance.y);
                        if let Some((bld_entity, _, bld_name)) = building {
                            if let Ok(docs) = item_params.p2().get_mut(*bld_entity) {
                                found_content = docs.documents.get(doc_title).cloned();
                            }
                            if found_content.is_some() {
                                found_location = bld_name.clone();
                            }
                        }
                    }

                    if let Some(content) = found_content {
                        if let Some(session) = sessions.sessions.get(&entity) {
                            let _ = session.prompt_tx.try_send(format!(
                                "=== DOCUMENT: {} ===\n{}\n=== END DOCUMENT ===",
                                doc_title,
                                preview_document(&content),
                            ));
                        }
                        thought.0 =
                            format!("Reading document '{}' from {}.", doc_title, found_location);
                    } else {
                        thought.0 = format!(
                            "ERROR: Could not find document '{}'. Carry it or stand at the building holding it.",
                            doc_title
                        );
                    }
                } else if item_name == "document" {
                    let mut titles: Vec<String> = Vec::new();
                    if let Ok(docs) = item_params.p1().get_mut(entity) {
                        titles.extend(docs.documents.keys().cloned());
                    }
                    if titles.is_empty() {
                        let building = structure_list
                            .iter()
                            .find(|(_, entrance, _)| pos.x == entrance.x && pos.y == entrance.y);
                        if let Some((bld_entity, _, _)) = building {
                            if let Ok(docs) = item_params.p2().get_mut(*bld_entity) {
                                titles.extend(docs.documents.keys().cloned());
                            }
                        }
                    }
                    titles.sort();

                    if let Some(title) = titles.first() {
                        if let Some(session) = sessions.sessions.get(&entity) {
                            let _ = session.prompt_tx.try_send(format!(
                                "Generic 'document' selected. Available document: '{}'. Use inspect_item with service='doc:{}' for the full contents, or take_item with service='document' to pick up the first one.",
                                title,
                                title,
                            ));
                        }
                        thought.0 = format!("Available document: {}", title);
                    } else {
                        thought.0 = "ERROR: No documents available here to inspect.".into();
                    }
                } else {
                    thought.0 = format!(
                        "Nothing special about '{}'. It's just a regular item.",
                        item_name
                    );
                }
            }

            "cancel_bounty" => {
                let at_board = known_locs
                    .locations
                    .values()
                    .find(|l| l.name == "bounty_board")
                    .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);

                if !at_board {
                    thought.0 = "Must be at the bounty board to cancel a bounty.".into();
                } else {
                    let active = bounty_registry
                        .tokens
                        .values()
                        .find(|b| {
                            b.claimed_by == Some(entity)
                                && b.state == crate::world::bounty::BountyState::Claimed
                        })
                        .map(|b| b.id);

                    if let Some(bounty_id) = active {
                        // Return bounty to available.
                        if let Some(b) = bounty_registry.tokens.get_mut(&bounty_id) {
                            b.state = crate::world::bounty::BountyState::Available;
                            b.claimed_by = None;
                            b.picked_up_tick = None;
                        }

                        // Clear the dropbox slot and return all items to agent.
                        // We queue the returns via PendingClaimItems since we don't have
                        // inventory access in this query.
                        if let Some(slot) = dropbox.clear_slot(entity) {
                            if let Some((board_entity, _, _)) = structure_list
                                .iter()
                                .find(|(_, _, name)| name == "bounty_board")
                            {
                                if let Ok(mut board_items) = item_params.p0().get_mut(*board_entity)
                                {
                                    if let Some(item_entity) = slot.bounty_token_item {
                                        board_items.remove(item_entity);
                                    }
                                    for item_entity in &slot.document_items {
                                        board_items.remove(*item_entity);
                                    }
                                }
                            }
                            if let Ok(mut contained) = item_params.p0().get_mut(entity) {
                                queue_dropbox_return(
                                    &mut commands,
                                    entity,
                                    &mut contained,
                                    slot,
                                    None,
                                );
                            }
                        }

                        *goal = AgentGoal::Idle;
                        thought.0 =
                            "Bounty cancelled. Token and items returned to your inventory.".into();

                        event_log.push(LogEvent {
                            tick: tick.0,
                            agent: name.0.clone(),
                            kind: LogKind::Action,
                            text: "Cancelled bounty — token and items returned.".into(),
                        });
                    } else {
                        thought.0 = "You don't have an active bounty to cancel.".into();
                    }
                }
            }

            "consume_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("");
                let item_type = crate::agents::trading::parse_item_type(item_name);

                if item_name.is_empty() || item_type.is_none() {
                    thought.0 = format!("ERROR: consume_item requires service=<item name>. Consumable items: coffee, muffin, rations, sandwich, soup.");
                } else {
                    let item = item_type.unwrap();
                    let has_item = agent_inventories_mcp
                        .get(entity)
                        .map(|inv| inv.has(item, 1))
                        .unwrap_or(false);

                    if !has_item {
                        thought.0 = format!("ERROR: You don't have any {} to consume.", item_name);
                    } else {
                        // Remove from inventory.
                        if let Ok(mut inv) = agent_inventories_mcp.get_mut(entity) {
                            inv.remove(item, 1);
                        }

                        // Apply effects based on item type.
                        match item {
                            crate::items::ItemType::Coffee => {
                                // Coffee boosts context ceiling by 10k tokens.
                                if let Ok(mut ctx) = agent_context_windows.get_mut(entity) {
                                    ctx.context_limit += 10_000;
                                    let ratio = ctx.tokens_used as f32 / ctx.context_limit as f32;
                                    needs.energy = (100.0 * (1.0 - ratio)).clamp(0.0, 100.0);
                                    thought.0 = format!(
                                        "Drank coffee! Context limit boosted to {}k tokens. Energy now {:.0}.",
                                        ctx.context_limit / 1000,
                                        needs.energy
                                    );
                                }
                            }
                            crate::items::ItemType::Muffin => {
                                needs.hunger = (needs.hunger + 30.0).min(100.0);
                                thought.0 = format!("Ate a muffin. Hunger now {:.0}.", needs.hunger);
                            }
                            crate::items::ItemType::Rations => {
                                needs.hunger = (needs.hunger + 50.0).min(100.0);
                                thought.0 = format!("Ate rations. Hunger now {:.0}.", needs.hunger);
                            }
                            crate::items::ItemType::Sandwich => {
                                needs.hunger = (needs.hunger + 60.0).min(100.0);
                                needs.boredom = (needs.boredom + 10.0).min(100.0);
                                thought.0 = format!("Ate a sandwich. Hunger now {:.0}.", needs.hunger);
                            }
                            crate::items::ItemType::Soup => {
                                needs.hunger = (needs.hunger + 45.0).min(100.0);
                                thought.0 = format!("Ate soup. Hunger now {:.0}.", needs.hunger);
                            }
                            _ => {
                                // Non-consumable — put it back.
                                if let Ok(mut inv) = agent_inventories_mcp.get_mut(entity) {
                                    inv.add(item, 1);
                                }
                                thought.0 = format!("You can't consume {}.", item_name);
                            }
                        }

                        event_log.push(LogEvent {
                            tick: tick.0,
                            agent: name.0.clone(),
                            kind: LogKind::Action,
                            text: format!("Consumed {}", item_name),
                        });
                    }
                }
            }

            "search_library" => {
                let at_library = structure_list.iter().any(|(_, entrance, sname)| {
                    sname == "library" && pos.x == entrance.x && pos.y == entrance.y
                });

                if !at_library {
                    thought.0 = "ERROR: You must be at the library to search documents. Use go_to_service with building='library'.".into();
                } else {
                    let query = mcp_action
                        .service
                        .as_deref()
                        .or(mcp_action.text.as_deref())
                        .unwrap_or("");

                    if query.is_empty() {
                        if library.documents.is_empty() {
                            thought.0 =
                                "The library is empty — no documents have been archived yet."
                                    .into();
                        } else {
                            let listing: Vec<String> = library
                                .documents
                                .iter()
                                .enumerate()
                                .map(|(i, doc)| {
                                    format!(
                                        "{}. \"{}\" by {} (bounty: {})",
                                        i + 1,
                                        doc.title,
                                        doc.author,
                                        doc.bounty_description
                                    )
                                })
                                .collect();
                            let msg = format!("=== LIBRARY CATALOG ({} documents) ===\n{}\n=== Use copy_document with service=<title> to copy one ===",
                                library.documents.len(), listing.join("\n"));
                            if let Some(session) = sessions.sessions.get(&entity) {
                                let _ = session.prompt_tx.try_send(msg);
                            }
                            thought.0 = format!(
                                "Found {} documents in the library.",
                                library.documents.len()
                            );
                        }
                    } else {
                        let query_lower = query.to_lowercase();
                        let matches: Vec<&crate::world::bounty::LibraryEntry> = library
                            .documents
                            .iter()
                            .filter(|doc| {
                                doc.title.to_lowercase().contains(&query_lower)
                                    || doc.author.to_lowercase().contains(&query_lower)
                                    || doc.bounty_description.to_lowercase().contains(&query_lower)
                                    || doc.content.to_lowercase().contains(&query_lower)
                            })
                            .collect();

                        if matches.is_empty() {
                            thought.0 =
                                format!("No documents matching '{}' in the library.", query);
                        } else {
                            let listing: Vec<String> = matches
                                .iter()
                                .map(|doc| {
                                    format!(
                                        "- \"{}\" by {} (bounty: {})",
                                        doc.title, doc.author, doc.bounty_description
                                    )
                                })
                                .collect();
                            let msg = format!("=== SEARCH RESULTS for '{}' ({} matches) ===\n{}\n=== Use copy_document with service=<title> to copy one ===",
                                query, matches.len(), listing.join("\n"));
                            if let Some(session) = sessions.sessions.get(&entity) {
                                let _ = session.prompt_tx.try_send(msg);
                            }
                            thought.0 =
                                format!("Found {} documents matching '{}'.", matches.len(), query);
                        }
                    }

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: name.0.clone(),
                        kind: LogKind::Action,
                        text: format!(
                            "Searched library for '{}'",
                            mcp_action.service.as_deref().unwrap_or("all")
                        ),
                    });
                }
            }

            "copy_document" => {
                let at_library = structure_list.iter().any(|(_, entrance, sname)| {
                    sname == "library" && pos.x == entrance.x && pos.y == entrance.y
                });

                if !at_library {
                    thought.0 = "ERROR: You must be at the library to copy documents.".into();
                } else {
                    let title = mcp_action.service.as_deref().unwrap_or("");
                    if title.is_empty() {
                        thought.0 =
                            "ERROR: copy_document requires service=<document title>.".into();
                    } else {
                        let found = library.documents.iter().find(|doc| doc.title == title);
                        if let Some(doc) = found {
                            commands.entity(entity).insert(PendingCreateDocument {
                                title: doc.title.clone(),
                                content: doc.content.clone(),
                            });

                            thought.0 =
                                format!("Copied '{}' from the library to your inventory.", title);

                            if let Some(session) = sessions.sessions.get(&entity) {
                                let _ = session.prompt_tx.try_send(format!(
                                    "=== LIBRARY COPY: {} ===\nAuthor: {}\nBounty: {}\n\n{}\n=== END ===",
                                    doc.title, doc.author, doc.bounty_description, doc.content,
                                ));
                            }

                            event_log.push(LogEvent {
                                tick: tick.0,
                                agent: name.0.clone(),
                                kind: LogKind::Action,
                                text: format!("Copied '{}' from library", title),
                            });
                        } else {
                            thought.0 = format!("ERROR: No document titled '{}' in the library. Use search_library to browse.", title);
                        }
                    }
                }
            }

            "help" => {
                let feedback = mcp_action
                    .feedback
                    .clone()
                    .or_else(|| mcp_action.text.clone())
                    .unwrap_or_else(|| "general feedback".into());

                suggestion_box.entries.push(Suggestion {
                    tick: tick.0,
                    agent: name.0.clone(),
                    text: feedback.clone(),
                });

                thought.0 = "Submitted feedback to the developers.".into();

                event_log.push(LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: LogKind::System,
                    text: format!("FEEDBACK: {}", feedback),
                });

                tracing::warn!("[FEEDBACK:{}] {}", name.0, feedback);
            }

            _ => {
                thought.0 = format!("Unknown action: {}", action_str);
            }
        }
    }
}

pub fn cleanup_abandoned_dropbox_slots_system(
    mut commands: Commands,
    mut boards: Query<
        (
            Entity,
            &BountyTokenStore,
            &mut BountyDropbox,
            &Entrance,
            &mut crate::items::ContainedItems,
        ),
        (With<BountyBoard>, Without<AgentName>),
    >,
    mut agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &mut crate::items::ContainedItems,
        &mut ThoughtBubble,
    ),
        (With<AgentName>, Without<BountyBoard>)>,
    mut event_log: ResMut<AgentEventLog>,
    tick: Res<TickCount>,
) {
    let Some((board_entity, bounty_registry, mut dropbox, board_entrance, mut board_items)) =
        boards.iter_mut().next()
    else {
        return;
    };

    let to_return: Vec<Entity> = dropbox
        .slots
        .iter()
        .filter_map(|(entity, slot)| {
            let pending_verification = slot.bounty_token_id.is_some_and(|bounty_id| {
                bounty_registry.get(bounty_id).is_some_and(|bounty| {
                    bounty.state == crate::world::bounty::BountyState::PendingVerification
                })
            });
            if pending_verification {
                return None;
            }

            agents
                .get(*entity)
                .ok()
                .filter(|(_, _, pos, _, _)| **pos != board_entrance.0)
                .map(|(entity, _, _, _, _)| entity)
        })
        .collect();

    for entity in to_return {
        let Some(slot) = dropbox.clear_slot(entity) else {
            continue;
        };
        let bounty = slot
            .bounty_token_id
            .and_then(|bounty_id| bounty_registry.get(bounty_id));
        if let Some(item_entity) = slot.bounty_token_item {
            board_items.remove(item_entity);
        }
        for item_entity in &slot.document_items {
            board_items.remove(*item_entity);
        }
        if let Ok((_, _, _, mut contained, _)) = agents.get_mut(entity) {
            queue_dropbox_return(&mut commands, entity, &mut contained, slot, bounty);
        }

        if let Ok((_, name, _, _, mut thought)) = agents.get_mut(entity) {
            thought.0 =
                "You left the bounty board, so your temporary dropbox contents were returned to your inventory."
                    .into();
            event_log.push(LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: LogKind::System,
                text: "DROPBOX RETURNED: left bounty board before submission".into(),
            });
        }
    }
}
