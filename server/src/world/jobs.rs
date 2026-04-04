use bevy::prelude::*;
use uuid::Uuid;

use crate::tick::TickCount;
use crate::world::bounty::{Bounty, BountyObjective, BountyRegistry, BountyState};
use crate::world::structures::{Entrance, SpriteType, StructureId};
use crate::items::ItemType;

/// A building that can offer jobs.
#[derive(Component)]
pub struct Employer {
    pub jobs: Vec<JobTemplate>,
    /// Ticks between posting new jobs.
    pub post_interval: u32,
    pub last_posted_tick: u64,
}

#[derive(Debug, Clone)]
pub struct JobTemplate {
    pub title: String,
    pub pay_gold: u32,
    /// How long the work takes in ticks.
    pub work_duration: u32,
}

/// System: buildings periodically post job openings as bounties.
pub fn job_posting_system(
    tick: Res<TickCount>,
    mut employers: Query<(&SpriteType, &Entrance, &mut Employer)>,
    mut bounty_registry: ResMut<BountyRegistry>,
) {
    for (sprite, entrance, mut employer) in &mut employers {
        if tick.0 - employer.last_posted_tick < employer.post_interval as u64 {
            continue;
        }

        // Check if there's already an available job bounty from this building.
        let has_open = bounty_registry.bounties.iter().any(|b| {
            b.state == BountyState::Available
                && b.description.contains(&sprite.0)
        });

        if has_open {
            continue;
        }

        // Pick a random job template.
        if let Some(job) = employer.jobs.first() {
            let bounty = Bounty {
                id: Uuid::new_v4(),
                description: format!("{} at {}", job.title, sprite.0),
                // Jobs use a special objective — agent goes to the building and works.
                objective: BountyObjective::WorkAtBuilding,
                reward_gold: job.pay_gold,
                state: BountyState::Available,
                claimed_by: None,
                claim_items: vec![],
            };

            tracing::info!("Job posted: {} ({} gold)", bounty.description, bounty.reward_gold);
            bounty_registry.bounties.push(bounty);
            employer.last_posted_tick = tick.0;
        }
    }
}
