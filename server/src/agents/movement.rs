use bevy::prelude::*;

use super::components::*;
use crate::world::map::GridPos;

/// Advances agents along their path based on speed.
/// Each tick of the MoveTimer, the agent moves one tile.
pub fn movement_system(
    time: Res<Time>,
    mut agents: Query<(
        &AgentName,
        &mut GridPos,
        &mut AgentAnimation,
        &mut MoveTimer,
        Option<&mut Path>,
    )>,
) {
    for (name, mut pos, mut anim, mut move_timer, path) in &mut agents {
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
        for _ in 0..move_timer.0.times_finished_this_tick() {
            if let Some(next) = path.0.pop_front() {
                *pos = next;
                anim.0 = AnimState::Walking;
            }
        }
    }
}
