use bevy::prelude::*;

use super::components::AgentGoal;

/// Agent needs — all values 0..100. At 0, the agent is in critical state.
#[derive(Component, Debug, Clone)]
pub struct Needs {
    pub energy: f32,
    pub hunger: f32,
    pub boredom: f32,
}

impl Default for Needs {
    fn default() -> Self {
        Self {
            energy: 80.0,
            hunger: 80.0,
            boredom: 60.0,
        }
    }
}

impl Needs {
    pub fn clamp(&mut self) {
        self.energy = self.energy.clamp(0.0, 100.0);
        self.hunger = self.hunger.clamp(0.0, 100.0);
        self.boredom = self.boredom.clamp(0.0, 100.0);
    }

    /// The most urgent need, if any is below the threshold.
    pub fn most_urgent(&self, threshold: f32) -> Option<NeedType> {
        let mut worst = None;
        let mut worst_val = threshold;

        if self.energy < worst_val {
            worst = Some(NeedType::Energy);
            worst_val = self.energy;
        }
        if self.hunger < worst_val {
            worst = Some(NeedType::Hunger);
            worst_val = self.hunger;
        }
        if self.boredom < worst_val {
            worst = Some(NeedType::Boredom);
            let _ = worst_val;
        }
        worst
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedType {
    Energy,
    Hunger,
    Boredom,
}

// Decay rates per tick (at 10 ticks/sec).
// Tuned so agents can work for ~5 min before needing to address needs.
const ENERGY_DECAY: f32 = 0.03;   // ~333 sec (~5.5 min) to drain fully
const HUNGER_DECAY: f32 = 0.025;  // ~400 sec (~6.7 min) to drain
const BOREDOM_DECAY_IDLE: f32 = 0.05; // ~200 sec (~3.3 min) when idle

/// System: decay needs every tick.
pub fn needs_decay_system(
    mut agents: Query<(&mut Needs, &AgentGoal)>,
) {
    for (mut needs, goal) in &mut agents {
        needs.energy -= ENERGY_DECAY;

        needs.hunger -= HUNGER_DECAY;

        // Boredom only decays when idle or wandering.
        match goal {
            AgentGoal::Idle | AgentGoal::Wandering | AgentGoal::WaitingAtBoard => {
                needs.boredom -= BOREDOM_DECAY_IDLE;
            }
            _ => {
                // Working/moving/executing — slightly raise boredom satisfaction.
                needs.boredom += 0.02;
            }
        }

        needs.clamp();
    }
}
