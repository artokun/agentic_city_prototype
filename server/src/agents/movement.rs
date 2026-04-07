use bevy::prelude::*;

use super::action_log::{ActionEvent, ActionLog};
use super::components::*;
use super::conversation::ActiveConversation;
use crate::tick::TickCount;
use crate::world::map::GridPos;
use crate::world::structures::{Entrance, SpriteType, StructureId};

/// Advances agents along their path based on speed.
/// Each tick of the MoveTimer, the agent moves one tile.
/// Agents in an active conversation do not move.
pub fn movement_system(
    time: Res<Time>,
    tick: Res<TickCount>,
    mut agents: Query<(
        &AgentName,
        &mut GridPos,
        &mut AgentAnimation,
        &mut MoveTimer,
        Option<&mut Path>,
        Option<&ActiveConversation>,
        &mut ActionLog,
    )>,
    structures: Query<(&SpriteType, &Entrance), With<StructureId>>,
) {
    for (_name, mut pos, mut anim, mut move_timer, path, active_convo, mut action_log) in
        &mut agents
    {
        // Agents can walk and talk simultaneously.
        // Previously conversations froze movement, causing starvation death spirals.
        move_timer.0.tick(time.delta());

        let Some(mut path) = path else {
            // No path — stay idle
            if anim.0 != AnimState::Idle {
                anim.0 = AnimState::Idle;
            }
            continue;
        };

        if path.0.is_empty() {
            if anim.0 == AnimState::Walking {
                anim.0 = AnimState::Idle;
            }
            continue;
        }

        // Move one tile each time the timer fires
        let mut moved = false;
        for _ in 0..move_timer.0.times_finished_this_tick() {
            if let Some(next) = path.0.pop_front() {
                *pos = next;
                anim.0 = AnimState::Walking;
                moved = true;
            }
        }

        if moved {
            for (sprite, entrance) in &structures {
                if *pos == entrance.0 {
                    let already_logged = action_log.entries.last().is_some_and(|entry| {
                        entry.tick == tick.0
                            && matches!(
                                &entry.event,
                                ActionEvent::EnteredBuilding { building } if building == &sprite.0
                            )
                    });
                    if !already_logged {
                        action_log.log(
                            tick.0,
                            ActionEvent::EnteredBuilding {
                                building: sprite.0.clone(),
                            },
                        );
                    }
                }
            }
        }
    }
}
