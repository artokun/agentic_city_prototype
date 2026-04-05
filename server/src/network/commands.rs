use bevy::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::items::ItemType;
use crate::network::action_handler::{MpcAction, PendingActions};
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyStep, StepCondition};

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

/// System: drain commands from the REST API and apply them to the world.
pub fn process_commands_system(
    mut receiver: ResMut<CommandReceiver>,
    mut bounty_registry: ResMut<BountyRegistry>,
    mut pending_actions: ResMut<PendingActions>,
    tick: Res<crate::tick::TickCount>,
) {
    while let Ok(cmd) = receiver.rx.try_recv() {
        match cmd {
            GameCommand::AgentAction { action_json } => {
                // Parse and queue for the action handler system.
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&action_json) {
                    pending_actions.actions.push(MpcAction {
                        agent_name: val.get("agent_name").and_then(|a| a.as_str()).unwrap_or("").into(),
                        agent_id: val.get("agent_id").and_then(|a| a.as_str()).unwrap_or("").into(),
                        action: val.get("action").and_then(|a| a.as_str()).unwrap_or("").into(),
                        building: val.get("building").and_then(|b| b.as_str()).map(|s| s.into()),
                        service: val.get("service").and_then(|s| s.as_str()).map(|s| s.into()),
                        agent_target: val.get("agent").and_then(|a| a.as_str()).map(|s| s.into()),
                        text: val.get("text").and_then(|t| t.as_str()).map(|s| s.into()),
                        feedback: val.get("feedback").and_then(|f| f.as_str()).map(|s| s.into()),
                    });
                    tracing::info!("[MCP] Queued action from {}",
                        val.get("agent_name").and_then(|a| a.as_str()).unwrap_or("?"));
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

                tracing::info!(
                    "New bounty created via API: {} ({} gold)",
                    bounty.description,
                    reward_gold,
                );

                bounty_registry.bounties.push(bounty);
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

                tracing::info!(
                    "Contract created: '{}' ({} gold, {} ticks TTL)",
                    title, reward_gold, ttl_ticks,
                );

                bounty_registry.bounties.push(bounty);
            }
        }
    }
}
