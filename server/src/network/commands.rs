use bevy::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::components::AgentName;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::items::{Inventory, ItemType};
use crate::network::action_handler::{MpcAction, PendingActions};
use crate::world::bounty::{
    Bounty, BountyBoard, BountyObjective, BountyStep, BountyTokenStore, StepCondition,
};

/// Commands sent from Axum REST handlers into Bevy.
#[derive(Debug)]
pub enum GameCommand {
    CreateBounty {
        id: Uuid,
        description: String,
        reward_gold: u32,
        objective: Option<String>,
    },
    AgentAction {
        action_json: String,
    },
    CreateContract {
        id: Uuid,
        title: String,
        description: String,
        reward_gold: u32,
        ttl_ticks: u32,
        steps: Vec<ContractStep>,
    },
    GmVerdict {
        bounty_id: String,
        approved: bool,
        reason: String,
    },
    DeliverDocument {
        agent_name: String,
        title: String,
        content: String,
    },
    GrantGold {
        agent_name: String,
        amount: u32,
        reason: String,
        message: Option<String>,
    },
    GmBroadcast {
        message: String,
    },
    GmDirectMessage {
        agent_name: String,
        message: String,
    },
    GrantItem {
        agent_name: String,
        item: String,
        quantity: u32,
    },
    ModifyNeed {
        agent_name: String,
        need: String,
        amount: f32,
    },
}

/// Step definition from the API.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ContractStep {
    pub description: String,
    #[serde(rename = "type")]
    pub step_type: String,
    pub building: Option<String>,
    pub service: Option<String>,
    pub amount: Option<u32>,
    pub title: Option<String>,
    pub min_count: Option<usize>,
}

/// Bevy resource holding the receiver end of the command channel.
#[derive(Resource)]
pub struct CommandReceiver {
    pub rx: mpsc::Receiver<GameCommand>,
}

/// Resource: pending GM verdicts that need inventory access to process payouts.
#[derive(Resource, Default)]
pub struct PendingVerdicts {
    pub verdicts: Vec<(Uuid, bool, String)>,
}

/// Resource: pending documents to deliver to agents.
#[derive(Resource, Default)]
pub struct PendingDocuments {
    pub docs: Vec<(String, String, String)>, // (agent_name, title, content)
}

/// Resource: pending discretionary gold grants from the System AI.
#[derive(Resource, Default)]
pub struct PendingGoldGrants {
    pub grants: Vec<(String, u32, String, Option<String>)>,
}

/// Resource: pending GM broadcast messages.
#[derive(Resource, Default)]
pub struct PendingGmBroadcasts {
    pub messages: Vec<String>,
}

/// Resource: pending GM direct messages.
#[derive(Resource, Default)]
pub struct PendingGmDirectMessages {
    pub messages: Vec<(String, String)>, // (agent_name, message)
}

/// Resource: pending GM item grants.
#[derive(Resource, Default)]
pub struct PendingGmItemGrants {
    pub grants: Vec<(String, String, u32)>, // (agent_name, item_name, quantity)
}

/// Resource: pending GM need modifications.
#[derive(Resource, Default)]
pub struct PendingGmNeedMods {
    pub mods: Vec<(String, String, f32)>, // (agent_name, need, amount)
}

/// System: drain commands from the REST API and apply them to the world.
pub fn process_commands_system(
    mut receiver: ResMut<CommandReceiver>,
    mut boards: Query<&mut BountyTokenStore, With<BountyBoard>>,
    mut pending_actions: ResMut<PendingActions>,
    mut pending_verdicts: ResMut<PendingVerdicts>,
    mut pending_docs: ResMut<PendingDocuments>,
    mut pending_gold_grants: ResMut<PendingGoldGrants>,
    mut pending_broadcasts: ResMut<PendingGmBroadcasts>,
    mut pending_dms: ResMut<PendingGmDirectMessages>,
    mut pending_item_grants: ResMut<PendingGmItemGrants>,
    mut pending_need_mods: ResMut<PendingGmNeedMods>,
    tick: Res<crate::tick::TickCount>,
) {
    let Some(mut bounty_registry) = boards.iter_mut().next() else {
        return;
    };
    while let Ok(cmd) = receiver.rx.try_recv() {
        match cmd {
            GameCommand::AgentAction { action_json } => {
                // Parse and queue for the action handler system.
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&action_json) {
                    pending_actions.actions.push(MpcAction {
                        agent_name: val
                            .get("agent_name")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .into(),
                        agent_id: val
                            .get("agent_id")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .into(),
                        action: val
                            .get("action")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .into(),
                        building: val
                            .get("building")
                            .and_then(|b| b.as_str())
                            .map(|s| s.into()),
                        service: val
                            .get("service")
                            .and_then(|s| s.as_str())
                            .map(|s| s.into()),
                        agent_target: val.get("agent").and_then(|a| a.as_str()).map(|s| s.into()),
                        text: val.get("text").and_then(|t| t.as_str()).map(|s| s.into()),
                        feedback: val
                            .get("feedback")
                            .and_then(|f| f.as_str())
                            .map(|s| s.into()),
                        x: val.get("x").and_then(|v| v.as_i64()).map(|v| v as i32),
                        y: val.get("y").and_then(|v| v.as_i64()).map(|v| v as i32),
                    });
                    tracing::debug!(
                        "[MCP] Queued action from {}",
                        val.get("agent_name")
                            .and_then(|a| a.as_str())
                            .unwrap_or("?")
                    );
                }
            }

            GameCommand::CreateBounty {
                id,
                description,
                reward_gold,
                objective,
            } => {
                // Parse objective type. Default to a generic "task" bounty.
                let obj = match objective.as_deref() {
                    Some("hide_item") => BountyObjective::HideItem(ItemType::GoldEgg),
                    Some("find_item") => BountyObjective::FindItem(ItemType::GoldEgg),
                    Some("work") => BountyObjective::WorkAtBuilding,
                    _ => {
                        // Generic task bounty — treat as work.
                        BountyObjective::WorkAtBuilding
                    }
                };

                let claim_items = match &obj {
                    BountyObjective::HideItem(item) => vec![(*item, 1)],
                    _ => vec![],
                };

                let bounty = Bounty::simple(id, description, obj, reward_gold, claim_items);

                tracing::debug!(
                    "New bounty created via API: {} ({} gold)",
                    bounty.description,
                    reward_gold,
                );

                bounty_registry.tokens.insert(bounty.id, bounty);
            }

            GameCommand::CreateContract {
                id,
                title,
                description,
                reward_gold,
                ttl_ticks,
                steps,
            } => {
                let bounty_steps: Vec<BountyStep> = steps
                    .into_iter()
                    .map(|s| {
                        let condition = match s.step_type.as_str() {
                            "spend_gold" => StepCondition::SpendGold {
                                building: s.building.unwrap_or_default(),
                                amount: s.amount.unwrap_or(1),
                            },
                            "use_service" => StepCondition::UseService {
                                service: s.service.unwrap_or_default(),
                            },
                            "web_search" => StepCondition::WebSearch {
                                min_count: s.min_count.unwrap_or(1),
                            },
                            "produce_document" => StepCondition::ProduceDocument {
                                title: s.title.unwrap_or_default(),
                            },
                            "visit_building" => StepCondition::VisitBuilding {
                                building: s.building.unwrap_or_default(),
                            },
                            "return_to_board" | _ => StepCondition::ReturnToBoard,
                        };
                        BountyStep {
                            description: s.description,
                            condition,
                        }
                    })
                    .collect();

                let bounty = Bounty::contract(
                    id,
                    description,
                    BountyObjective::WorkAtBuilding,
                    reward_gold,
                    tick.0,
                    ttl_ticks,
                    bounty_steps,
                );

                tracing::debug!(
                    "Contract created: '{}' ({} gold, {} ticks TTL)",
                    title,
                    reward_gold,
                    ttl_ticks,
                );

                bounty_registry.tokens.insert(bounty.id, bounty);
            }

            GameCommand::DeliverDocument {
                agent_name,
                title,
                content,
            } => {
                tracing::debug!(
                    "[DOC] Delivering '{}' to {} ({} chars)",
                    title,
                    agent_name,
                    content.len()
                );
                pending_docs.docs.push((agent_name, title, content));
            }

            GameCommand::GmVerdict {
                bounty_id,
                approved,
                reason,
            } => {
                if let Ok(uuid) = Uuid::parse_str(&bounty_id) {
                    tracing::debug!(
                        "[GM] Verdict for {}: approved={} reason={}",
                        bounty_id,
                        approved,
                        reason
                    );
                    pending_verdicts.verdicts.push((uuid, approved, reason));
                } else {
                    tracing::warn!("[GM] Invalid bounty ID: {}", bounty_id);
                }
            }

            GameCommand::GrantGold {
                agent_name,
                amount,
                reason,
                message,
            } => {
                tracing::debug!(
                    "[GM] Queued discretionary grant: {}g to {} ({})",
                    amount,
                    agent_name,
                    reason
                );
                pending_gold_grants
                    .grants
                    .push((agent_name, amount, reason, message));
            }

            GameCommand::GmBroadcast { message } => {
                tracing::info!("[GM] Broadcast: {}", message);
                pending_broadcasts.messages.push(message);
            }

            GameCommand::GmDirectMessage {
                agent_name,
                message,
            } => {
                tracing::info!("[GM] DM to {}: {}", agent_name, message);
                pending_dms.messages.push((agent_name, message));
            }

            GameCommand::GrantItem {
                agent_name,
                item,
                quantity,
            } => {
                tracing::info!(
                    "[GM] Grant item: {}x {} to {}",
                    quantity,
                    item,
                    agent_name
                );
                pending_item_grants.grants.push((agent_name, item, quantity));
            }

            GameCommand::ModifyNeed {
                agent_name,
                need,
                amount,
            } => {
                tracing::info!(
                    "[GM] Modify need: {} {} {:+.1}",
                    agent_name,
                    need,
                    amount
                );
                pending_need_mods.mods.push((agent_name, need, amount));
            }
        }
    }
}

/// System: process GM autonomous commands (broadcasts, DMs, item grants, need mods).
/// Runs after process_commands_system to handle the pending resources it populates.
pub fn process_gm_commands_system(
    mut pending_broadcasts: ResMut<PendingGmBroadcasts>,
    mut pending_dms: ResMut<PendingGmDirectMessages>,
    mut pending_item_grants: ResMut<PendingGmItemGrants>,
    mut pending_need_mods: ResMut<PendingGmNeedMods>,
    mut event_log: ResMut<AgentEventLog>,
    agent_sessions: Res<crate::agents::ai::AgentSessions>,
    mut agents: Query<(Entity, &AgentName, &mut Inventory, &mut Needs)>,
    tick: Res<crate::tick::TickCount>,
) {
    // --- Broadcasts ---
    for message in pending_broadcasts.messages.drain(..) {
        event_log.push(LogEvent {
            tick: tick.0,
            agent: "SYSTEM".into(),
            kind: LogKind::Speech,
            text: format!("[BROADCAST] {}", message),
        });
        let formatted = format!("[SYSTEM BROADCAST] {}", message);
        for (_entity, _name, _inv, _needs) in agents.iter() {
            // We iterate agents to log, but send via sessions below.
        }
        for (_entity, _session) in agent_sessions.sessions.iter() {
            let _ = _session.prompt_tx.try_send(formatted.clone());
        }
        tracing::info!("[GM] Broadcast sent to {} sessions", agent_sessions.sessions.len());
    }

    // --- Direct Messages ---
    for (target_name, message) in pending_dms.messages.drain(..) {
        let mut found = false;
        for (entity, name, _inv, _needs) in agents.iter() {
            if name.0.eq_ignore_ascii_case(&target_name) {
                if let Some(session) = agent_sessions.sessions.get(&entity) {
                    let formatted = format!("[SYSTEM DM] {}", message);
                    let _ = session.prompt_tx.try_send(formatted);
                    found = true;
                }
                break;
            }
        }
        event_log.push(LogEvent {
            tick: tick.0,
            agent: "SYSTEM".into(),
            kind: LogKind::Speech,
            text: format!("[to {}] {}", target_name, message),
        });
        if !found {
            tracing::warn!("[GM] DM target '{}' not found or has no session", target_name);
        }
    }

    // --- Item Grants ---
    for (target_name, item_name, quantity) in pending_item_grants.grants.drain(..) {
        let item_type = crate::network::action_handler::parse_item_name(&item_name);
        match item_type {
            Some(item) => {
                let mut granted = false;
                for (_entity, name, mut inv, _needs) in agents.iter_mut() {
                    if name.0.eq_ignore_ascii_case(&target_name) {
                        inv.add(item, quantity);
                        granted = true;
                        break;
                    }
                }
                if granted {
                    event_log.push(LogEvent {
                        tick: tick.0,
                        agent: "SYSTEM".into(),
                        kind: LogKind::System,
                        text: format!("[GM] Granted {}x {} to {}", quantity, item_name, target_name),
                    });
                } else {
                    tracing::warn!("[GM] Grant item target '{}' not found", target_name);
                }
            }
            None => {
                tracing::warn!("[GM] Unknown item type '{}' for grant", item_name);
            }
        }
    }

    // --- Need Modifications ---
    for (target_name, need_name, amount) in pending_need_mods.mods.drain(..) {
        let mut modified = false;
        for (_entity, name, _inv, mut needs) in agents.iter_mut() {
            if name.0.eq_ignore_ascii_case(&target_name) {
                match need_name.to_lowercase().as_str() {
                    "energy" => {
                        needs.energy = (needs.energy + amount).clamp(0.0, 100.0);
                    }
                    "hunger" => {
                        needs.hunger = (needs.hunger + amount).clamp(0.0, 100.0);
                    }
                    "boredom" => {
                        needs.boredom = (needs.boredom + amount).clamp(0.0, 100.0);
                    }
                    _ => {
                        tracing::warn!("[GM] Unknown need '{}' for modify_need", need_name);
                    }
                }
                modified = true;
                break;
            }
        }
        if modified {
            event_log.push(LogEvent {
                tick: tick.0,
                agent: "SYSTEM".into(),
                kind: LogKind::System,
                text: format!(
                    "[GM] Modified {}'s {} by {:+.1}",
                    target_name, need_name, amount
                ),
            });
        } else {
            tracing::warn!("[GM] Modify need target '{}' not found", target_name);
        }
    }
}
