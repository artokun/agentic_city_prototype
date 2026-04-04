use bevy::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::items::ItemType;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};

/// Commands sent from Axum REST handlers into Bevy.
#[derive(Debug)]
pub enum GameCommand {
    CreateBounty {
        id: Uuid,
        description: String,
        reward_gold: u32,
        objective: Option<String>,
    },
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
) {
    while let Ok(cmd) = receiver.rx.try_recv() {
        match cmd {
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

                bounty_registry.bounties.push(Bounty {
                    id,
                    description,
                    objective: obj,
                    reward_gold,
                    state: BountyState::Available,
                    claimed_by: None,
                    claim_items,
                });

                tracing::info!(
                    "New bounty created via API: {} ({} gold)",
                    bounty_registry.bounties.last().unwrap().description,
                    reward_gold,
                );
            }
        }
    }
}
