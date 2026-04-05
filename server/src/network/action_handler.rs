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
use crate::world::bounty::BountyRegistry;
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

/// Marker component: queued conversation message to apply to both agents' logs.
#[derive(Component)]
pub struct PendingConversationMessage {
    pub message: ConversationMessage,
    pub partner: Entity,
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
        commands.entity(entity).remove::<PendingConversationMessage>();
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
pub fn process_gm_verdicts_system(
    mut verdicts: ResMut<super::commands::PendingVerdicts>,
    mut bounty_registry: ResMut<crate::world::bounty::BountyRegistry>,
    mut agents: Query<(Entity, &AgentName, &mut Inventory, &mut ThoughtBubble)>,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
    sessions: Res<crate::agents::ai::AgentSessions>,
) {
    for (bounty_id, approved, reason) in verdicts.verdicts.drain(..) {
        let bounty = bounty_registry.bounties.iter().find(|b| b.id == bounty_id).cloned();
        let Some(bounty) = bounty else {
            tracing::warn!("[GM] Bounty {} not found", bounty_id);
            continue;
        };
        let Some(agent_entity) = bounty.claimed_by else {
            tracing::warn!("[GM] Bounty {} has no claimant", bounty_id);
            continue;
        };

        if approved {
            // Pay out.
            if let Ok((_, name, mut inv, mut thought)) = agents.get_mut(agent_entity) {
                inv.add(crate::items::ItemType::GoldCoin, bounty.reward_gold);
                thought.0 = format!("GM approved! Collected {} gold!", bounty.reward_gold);
                tracing::info!("[GM APPROVED] {} +{} gold for '{}'", name.0, bounty.reward_gold, bounty.description);

                event_log.push(crate::agents::event_log::LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: crate::agents::event_log::LogKind::System,
                    text: format!("Game Master APPROVED bounty '{}' — +{}g. Reason: {}", bounty.description, bounty.reward_gold, reason),
                });

                // Notify the agent.
                if let Some(session) = sessions.sessions.get(&agent_entity) {
                    let _ = session.prompt_tx.try_send(
                        format!("BOUNTY APPROVED by the Game Master! You earned {} gold for '{}'. Reason: {}", bounty.reward_gold, bounty.description, reason),
                    );
                }
            }
            // Mark completed.
            if let Some(b) = bounty_registry.bounties.iter_mut().find(|b| b.id == bounty_id) {
                b.state = crate::world::bounty::BountyState::Completed;
            }
        } else {
            // Rejected — bounty goes back to available.
            if let Ok((_, name, _, mut thought)) = agents.get_mut(agent_entity) {
                thought.0 = format!("GM rejected bounty: {}", reason);
                tracing::info!("[GM REJECTED] {} bounty '{}': {}", name.0, bounty.description, reason);

                event_log.push(crate::agents::event_log::LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: crate::agents::event_log::LogKind::System,
                    text: format!("Game Master REJECTED bounty '{}'. Reason: {}", bounty.description, reason),
                });

                if let Some(session) = sessions.sessions.get(&agent_entity) {
                    let _ = session.prompt_tx.try_send(
                        format!("BOUNTY REJECTED by the Game Master. Your submission for '{}' was not approved. Reason: {}. The bounty is back on the board.", bounty.description, reason),
                    );
                }
            }
            // Return bounty to available.
            if let Some(b) = bounty_registry.bounties.iter_mut().find(|b| b.id == bounty_id) {
                b.state = crate::world::bounty::BountyState::Available;
                b.claimed_by = None;
                b.picked_up_tick = None;
            }
        }
    }
}

/// System: deliver research documents to agents.
pub fn deliver_documents_system(
    mut pending: ResMut<super::commands::PendingDocuments>,
    mut agents: Query<(&AgentName, &mut Inventory, &mut crate::items::DocumentInventory, &mut ThoughtBubble)>,
    sessions: Res<crate::agents::ai::AgentSessions>,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
) {
    for (agent_name, title, content) in pending.docs.drain(..) {
        let agent = agents.iter_mut().find(|(n, _, _, _)| n.0 == agent_name);
        if let Some((name, mut inv, mut docs, mut thought)) = agent {
            // Add document to DocumentInventory with content.
            docs.add(title.clone(), content.clone());
            // Add Document item type to regular inventory.
            inv.add(crate::items::ItemType::Document, 1);

            thought.0 = format!("Research complete! Document '{}' is in your inventory.", title);
            tracing::info!("[DOC] {} received '{}' ({} chars)", name.0, title, content.len());

            event_log.push(crate::agents::event_log::LogEvent {
                tick: tick.0,
                agent: name.0.clone(),
                kind: crate::agents::event_log::LogKind::System,
                text: format!("Research document produced: '{}'", title),
            });

            // Notify the agent.
            // Find entity for session lookup — use a different approach.
            // We can't easily get Entity from this query pattern, so just log it.
        }
    }
}

/// System: process pending item deposits (agent → structure inventory transfer).
pub fn process_deposits_system(
    mut commands: Commands,
    deposits: Query<(Entity, &AgentName, &PendingDeposit)>,
    mut agent_inventories: Query<&mut Inventory, With<AgentName>>,
    mut structure_inventories: Query<&mut Inventory, (With<StructureId>, Without<AgentName>)>,
) {
    for (entity, name, deposit) in &deposits {
        let item_type = parse_item_name(&deposit.item_name);

        if let Some(item) = item_type {
            let mut success = false;
            if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                if agent_inv.has(item, 1) {
                    agent_inv.remove(item, 1);
                    if let Ok(mut bld_inv) = structure_inventories.get_mut(deposit.building_entity) {
                        bld_inv.add(item, 1);
                        success = true;
                        tracing::info!("[Deposit] {} deposited {} into building", name.0, deposit.item_name);
                    }
                } else {
                    tracing::info!("[Deposit] {} doesn't have {} in inventory", name.0, deposit.item_name);
                }
            }
            if !success {
                tracing::warn!("[Deposit] Failed: {} tried to deposit {} but doesn't have it", name.0, deposit.item_name);
            }
        } else {
            tracing::warn!("[Deposit] Unknown item type: {}", deposit.item_name);
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
        _ => None,
    }
}

/// System: process take_item — move item from structure to agent inventory.
pub fn process_take_items_system(
    mut commands: Commands,
    takes: Query<(Entity, &AgentName, &PendingTakeItem)>,
    mut agent_inventories: Query<&mut Inventory, With<AgentName>>,
    mut structure_inventories: Query<&mut Inventory, (With<StructureId>, Without<AgentName>)>,
) {
    for (entity, name, take) in &takes {
        let item_type = parse_item_name(&take.item_name);

        if let Some(item) = item_type {
            if let Ok(mut bld_inv) = structure_inventories.get_mut(take.building_entity) {
                if bld_inv.has(item, 1) {
                    bld_inv.remove(item, 1);
                    if let Ok(mut agent_inv) = agent_inventories.get_mut(entity) {
                        agent_inv.add(item, 1);
                        tracing::info!("[Take] {} took {} from building", name.0, take.item_name);
                    }
                } else {
                    tracing::info!("[Take] Building doesn't have {} for {}", take.item_name, name.0);
                }
            }
        } else {
            tracing::warn!("[Take] Unknown item: {}", take.item_name);
        }

        commands.entity(entity).remove::<PendingTakeItem>();
    }
}

/// System: process create_document — agent writes a document to their inventory.
pub fn process_create_documents_system(
    mut commands: Commands,
    pending: Query<(Entity, &AgentName, &AgentId, &PendingCreateDocument)>,
    mut inventories: Query<&mut Inventory, With<AgentName>>,
    mut doc_inventories: Query<&mut crate::items::DocumentInventory>,
) {
    for (entity, name, agent_id, doc) in &pending {
        // Add to DocumentInventory (with content).
        if let Ok(mut docs) = doc_inventories.get_mut(entity) {
            docs.add(doc.title.clone(), doc.content.clone());
        }
        // Add Document item type to regular inventory.
        if let Ok(mut inv) = inventories.get_mut(entity) {
            inv.add(crate::items::ItemType::Document, 1);
        }

        // Also save to filesystem.
        let agent_dir = format!("./documents/{}", name.0.to_lowercase());
        let _ = std::fs::create_dir_all(&agent_dir);
        let _ = std::fs::write(format!("{}/{}", agent_dir, doc.title), &doc.content);

        tracing::info!("[CreateDoc] {} created '{}' ({} chars)", name.0, doc.title, doc.content.len());
        commands.entity(entity).remove::<PendingCreateDocument>();
    }
}

/// System: auto-exchange business cards when agents are in a conversation.
/// Each agent adds their partner as a contact if not already known.
pub fn auto_exchange_cards_system(
    mut agents: Query<(&AgentName, &AgentId, &ActiveConversation, &mut BusinessCards)>,
) {
    // Collect partner IDs first to avoid borrow conflicts.
    let partner_ids: std::collections::HashMap<Entity, (String, String)> = agents.iter()
        .map(|(name, id, _convo, _)| (_convo.partner, (name.0.clone(), id.0.to_string())))
        .collect();

    for (_name, _id, convo, mut cards) in &mut agents {
        if !cards.contacts.contains_key(&convo.partner_name) && cards.cards_remaining > 0 {
            // Look up partner's ID from the pre-collected map.
            let partner_id = partner_ids.get(&convo.partner)
                .map(|(_, id)| id.clone())
                .unwrap_or_default();
            cards.contacts.insert(convo.partner_name.clone(), partner_id);
            cards.cards_remaining -= 1;
            tracing::info!("[Cards] {} received {}'s business card", _name.0, convo.partner_name);
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
    mut bounty_registry: ResMut<BountyRegistry>,
    sessions: Res<AgentSessions>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos,
        &mut AgentGoal, &mut ThoughtBubble,
        &KnownLocations, Option<&ShiftWorker>,
        &mut Needs,
        Option<&ActiveConversation>,
        &BusinessCards,
    )>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
    mut trade_proposals: Query<(Entity, &mut TradeProposal)>,
) {
    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter().map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone())).collect();

    // Pre-collect agent info for cross-agent lookups (avoids borrow conflicts).
    let agent_snapshot: Vec<(Entity, String, GridPos, bool)> = agents.iter()
        .map(|(e, n, p, _, _, _, _, _, ac, _)| (e, n.0.clone(), *p, ac.is_some()))
        .collect();

    for mcp_action in pending.actions.drain(..) {
        // Find the agent entity by name.
        let agent = agents.iter_mut()
            .find(|(_, name, _, _, _, _, _, _, _, _)| name.0 == mcp_action.agent_name);

        let Some((entity, name, pos, mut goal, mut thought, known_locs, shift_worker, mut needs, active_convo, business_cards)) = agent else {
            tracing::warn!("[MCP] Agent '{}' not found", mcp_action.agent_name);
            continue;
        };

        let action_str = &mcp_action.action;
        tracing::info!("[MCP:{}] action={} building={:?} service={:?}",
            name.0, action_str, mcp_action.building, mcp_action.service);

        event_log.push(LogEvent {
            tick: tick.0,
            agent: name.0.clone(),
            kind: LogKind::Action,
            text: format!("→ {} {:?} {:?}", action_str, mcp_action.building, mcp_action.service),
        });

        // Helper: find building and get location info for error messages.
        let find_building = |name: &str| -> Option<(Entity, GridPos, String)> {
            structure_list.iter().find(|(_, _, s)| s == name).map(|(e, p, s)| (*e, *p, s.clone()))
        };

        let known_buildings: Vec<String> = structure_list.iter().map(|(_, _, s)| s.clone()).collect();

        match action_str.as_str() {
            "go_to_board" => {
                // Preserve ReturningToBoard — agent is heading back to collect reward.
                if !matches!(*goal, AgentGoal::ReturningToBoard(_)) {
                    *goal = AgentGoal::GoingToBoard;
                }
                if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                    if let Some(p) = pathfinding::bfs(&map, *pos, board.entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        if matches!(*goal, AgentGoal::ReturningToBoard(_)) {
                            thought.0 = format!("Returning to board to collect bounty reward ({} tiles).", tiles);
                        } else {
                            thought.0 = format!("Heading to the bounty board ({} tiles).", tiles);
                        }
                    } else {
                        thought.0 = "ERROR: Can't find path to the bounty board!".into();
                    }
                } else {
                    thought.0 = "ERROR: Don't know where the bounty board is. Try look_around first.".into();
                }
            }

            "go_to_service" => {
                let building = mcp_action.building.as_deref().unwrap_or("unknown");
                let service = mcp_action.service.as_deref().unwrap_or("browse");

                if let Some((bld_entity, entrance, _)) = find_building(building) {
                    *goal = AgentGoal::GoingToService {
                        building: bld_entity,
                        service: service.into(),
                    };
                    if let Some(p) = pathfinding::bfs(&map, *pos, entrance) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        thought.0 = format!("Walking to {} for {} ({} tiles).", building, service, tiles);
                    } else {
                        thought.0 = format!("ERROR: Can't find path to {}. Try look_around.", building);
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
                        *goal = AgentGoal::Wandering;
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
                        // Start shift immediately.
                        *goal = AgentGoal::GoingToService {
                            building: bld_entity,
                            service: "work_shift".into(),
                        };
                        thought.0 = format!("Starting shift at {}.", building);
                    } else {
                        // Walk there first.
                        *goal = AgentGoal::GoingToService {
                            building: bld_entity,
                            service: "work_shift".into(),
                        };
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
                if matches!(*goal, AgentGoal::WorkingShift { .. }) {
                    *goal = AgentGoal::Idle;
                    thought.0 = "Left the shift. Paycheck earned based on ticks worked.".into();
                } else {
                    thought.0 = "ERROR: Not currently working a shift. Nothing to leave.".into();
                }
            }

            "complete_bounty" => {
                // Must be at the bounty board to submit completion.
                let at_board = known_locs.locations.values()
                    .find(|l| l.name == "bounty_board")
                    .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);

                if !at_board {
                    thought.0 = "You must be at the bounty board to submit a bounty for completion. Use go_to_board first.".into();
                } else {
                    let bounty_id = match *goal {
                        AgentGoal::ExecutingBounty(id) => Some(id),
                        AgentGoal::ReturningToBoard(id) => Some(id),
                        _ => {
                            bounty_registry.bounties.iter()
                                .find(|b| b.claimed_by == Some(entity) && b.state == crate::world::bounty::BountyState::Claimed)
                                .map(|b| b.id)
                        }
                    };

                    if let Some(bounty_id) = bounty_id {
                        // Submit for GM verification directly (already at board).
                        *goal = AgentGoal::ReturningToBoard(bounty_id);
                        thought.0 = "Bounty submitted for Game Master review! Waiting for verdict...".into();
                    } else {
                        thought.0 = "ERROR: No active bounty to complete. Claim one at the bounty board first.".into();
                    }
                }
            }

            "go_to" => {
                if let (Some(x), Some(y)) = (mcp_action.x, mcp_action.y) {
                    let target = GridPos { x, y };
                    if !map.is_walkable(&target) {
                        thought.0 = format!("ERROR: ({},{}) is not walkable (might be inside a building). Try nearby coordinates.", x, y);
                    } else if let Some(p) = pathfinding::bfs(&map, *pos, target) {
                        let tiles = p.len();
                        commands.entity(entity).insert(Path(p));
                        *goal = AgentGoal::Wandering;
                        thought.0 = format!("Walking to ({},{}) — {} tiles.", x, y, tiles);
                    } else {
                        thought.0 = format!("ERROR: Can't find path to ({},{}). The map is 40x40.", x, y);
                    }
                } else {
                    thought.0 = "ERROR: go_to requires x and y coordinates. Map is 40x40 (0-39).".into();
                }
            }

            "chat_with" => {
                let target = mcp_action.agent_target.as_deref().unwrap_or("someone");
                *goal = AgentGoal::Wandering;
                thought.0 = format!("Looking to chat with {}...", target);
            }

            "send_message" => {
                if let (Some(recipient), Some(text)) = (&mcp_action.agent_target, &mcp_action.text) {
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
                // Allow claiming if at the board (any goal state — the board queue
                // was too strict and agents kept getting stuck).
                let at_board = known_locs.locations.values()
                    .find(|l| l.name == "bounty_board")
                    .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);
                if !at_board && !matches!(*goal, AgentGoal::InteractingWithBoard) {
                    thought.0 = "Must be at the bounty board to claim a bounty! Go there first.".into();
                } else {
                    let keyword = mcp_action.text.as_deref()
                        .or(mcp_action.service.as_deref())
                        .unwrap_or("");

                    // Find available bounty by ID prefix or keyword match.
                    let bounty_match = bounty_registry.available().iter()
                        .find(|b| {
                            if keyword.is_empty() { return true; }
                            // Match by short ID (first 6 chars of UUID).
                            let short_id = &b.id.to_string()[..6];
                            if keyword == short_id || keyword == b.id.to_string() {
                                return true;
                            }
                            b.description.to_lowercase().contains(&keyword.to_lowercase())
                        })
                        .map(|b| b.id);

                    if let Some(bounty_id) = bounty_match {
                        if let Some(bounty) = bounty_registry.claim(bounty_id, entity, tick.0) {
                            let desc = bounty.description.clone();
                            let claim_items = bounty.claim_items.clone();

                            // Give claim items to agent via deferred component.
                            if !claim_items.is_empty() {
                                commands.entity(entity).insert(PendingClaimItems {
                                    items: claim_items,
                                });
                            }
                            // Extract agent instructions from hidden_criteria.
                            let instructions = bounty.hidden_criteria
                                .split("\n\nGM:")
                                .next()
                                .unwrap_or("")
                                .strip_prefix("Instructions for agent: ")
                                .unwrap_or("Complete the task and return to the board.")
                                .to_string();
                            thought.0 = format!("Claimed: {}. INSTRUCTIONS: {}", desc, instructions);

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
                            thought.0 = "Couldn't claim that bounty — it may already be taken.".into();
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
                *goal = AgentGoal::Idle;
                thought.0 = "Left the bounty board.".into();
            }

            "start_conversation" => {
                let target_name = mcp_action.agent_target.as_deref().unwrap_or("unknown");

                if active_convo.is_some() {
                    thought.0 = "ERROR: Already in a conversation. Use end_conversation first.".into();
                } else {
                    // Look up target from pre-collected snapshot (avoids borrow conflict).
                    let target_info = agent_snapshot.iter()
                        .find(|(_, n, _, _)| n == target_name)
                        .map(|(e, n, p, in_convo)| (*e, n.clone(), *p, *in_convo));

                    match target_info {
                        None => {
                            thought.0 = format!("ERROR: Agent '{}' not found.", target_name);
                        }
                        Some((_, _, _, true)) => {
                            thought.0 = format!("ERROR: {} is already in a conversation.", target_name);
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
                        let _ = session.prompt_tx.try_send(
                            format!("{} says: \"{}\"", self_name, message_text),
                        );
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
                    thought.0 = "ERROR: Not in a conversation. Use start_conversation first.".into();
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
                    commands.entity(partner_entity).remove::<ActiveConversation>();
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
                        let _ = session.prompt_tx.try_send(
                            format!("{} ended the conversation.", self_name),
                        );
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
                        (Ok(offered_items), Ok(requested_items)) if !offered_items.is_empty() || !requested_items.is_empty() => {
                            // Check for existing trade proposal between these agents.
                            let existing = trade_proposals.iter()
                                .any(|(_, tp)| {
                                    (tp.proposer == entity && tp.responder == partner_entity)
                                    || (tp.proposer == partner_entity && tp.responder == entity)
                                });

                            if existing {
                                thought.0 = "ERROR: A trade is already pending with this partner. Accept, reject, or wait.".into();
                            } else {
                                let offered_desc: Vec<String> = offered_items.iter().map(|i| i.to_string()).collect();
                                let requested_desc: Vec<String> = requested_items.iter().map(|i| i.to_string()).collect();

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
                let proposal = trade_proposals.iter_mut()
                    .find(|(_, tp)| tp.proposer == entity || tp.responder == entity);

                if let Some((_, mut tp)) = proposal {
                    if tp.proposer == entity {
                        tp.proposer_accepted = true;
                        thought.0 = "Accepted the trade on your side.".into();
                    } else {
                        tp.responder_accepted = true;
                        thought.0 = "Accepted the trade on your side.".into();
                    }

                    let partner = if tp.proposer == entity { tp.responder } else { tp.proposer };

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: self_name.clone(),
                        kind: LogKind::Action,
                        text: "Accepted trade.".into(),
                    });

                    // Notify the partner.
                    if let Some(session) = sessions.sessions.get(&partner) {
                        let _ = session.prompt_tx.try_send(
                            format!("{} accepted the trade!", self_name),
                        );
                    }
                } else {
                    thought.0 = "ERROR: No pending trade to accept.".into();
                }
            }

            "reject_trade" => {
                let self_name = name.0.clone();

                // Find and remove a trade proposal where this agent is a participant.
                let proposal = trade_proposals.iter()
                    .find(|(_, tp)| tp.proposer == entity || tp.responder == entity)
                    .map(|(e, tp)| (e, tp.proposer, tp.responder));

                if let Some((proposal_entity, proposer, responder)) = proposal {
                    let partner = if proposer == entity { responder } else { proposer };

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
                        let _ = session.prompt_tx.try_send(
                            format!("{} rejected the trade.", self_name),
                        );
                    }
                } else {
                    thought.0 = "ERROR: No pending trade to reject.".into();
                }
            }

            "deposit_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("unknown");

                // Must be inside a building.
                let building = structure_list.iter()
                    .find(|(_, entrance, name)| pos.x == entrance.x && pos.y == entrance.y);

                if let Some((bld_entity, _, bld_name)) = building {
                    commands.entity(entity).insert(PendingDeposit {
                        item_name: item_name.to_string(),
                        building_entity: *bld_entity,
                    });
                    thought.0 = format!("Depositing {} into {}.", item_name, bld_name);

                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: name.0.clone(),
                        kind: LogKind::Action,
                        text: format!("DEPOSIT: {} → {}", item_name, bld_name),
                    });
                } else {
                    thought.0 = "ERROR: Must be at a building entrance to deposit items. Go to a building first.".into();
                }
            }

            "take_item" => {
                let item_name = mcp_action.service.as_deref().unwrap_or("unknown");

                let building = structure_list.iter()
                    .find(|(_, entrance, _)| pos.x == entrance.x && pos.y == entrance.y);

                if let Some((bld_entity, _, bld_name)) = building {
                    commands.entity(entity).insert(PendingTakeItem {
                        item_name: item_name.to_string(),
                        building_entity: *bld_entity,
                    });
                    thought.0 = format!("Taking {} from {}.", item_name, bld_name);

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
                let title = mcp_action.service.as_deref().unwrap_or("untitled.md").to_string();
                let content = mcp_action.text.as_deref().unwrap_or("").to_string();

                if content.is_empty() {
                    thought.0 = "ERROR: create_document requires 'text' with the markdown content.".into();
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

            "help" => {
                let feedback = mcp_action.feedback.clone()
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
