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

use crate::config;

/// System: decay needs every tick.
pub fn needs_decay_system(
    mut agents: Query<(&mut Needs, &AgentGoal), Without<crate::world::hospital::Incapacitated>>,
) {
    for (mut needs, goal) in &mut agents {
        // Energy decay is driven by token usage (token_tracking::token_drain_system).

        needs.hunger -= config::hunger_decay();

        // Boredom only decays when idle or wandering.
        match goal {
            AgentGoal::Idle | AgentGoal::Wandering | AgentGoal::WaitingAtBoard => {
                needs.boredom -= config::boredom_decay_idle();
            }
            _ => {
                needs.boredom += config::boredom_recovery_active();
            }
        }

        needs.clamp();
    }
}

/// System: auto-consume food when hunger drops below critical threshold.
/// Safety net so agents don't starve while waiting for AI decisions or status updates.
pub fn auto_eat_system(
    mut agents: Query<
        (&mut Needs, &mut crate::items::Inventory, &crate::agents::components::AgentName),
        Without<crate::world::hospital::Incapacitated>,
    >,
    mut event_log: ResMut<crate::agents::event_log::AgentEventLog>,
    tick: Res<crate::tick::TickCount>,
) {
    use crate::items::ItemType;

    for (mut needs, mut inv, name) in &mut agents {
        if needs.hunger >= 15.0 {
            continue;
        }

        // Try to eat the best food available.
        let food_priority = [
            (ItemType::Sandwich, 60.0),
            (ItemType::Rations, 50.0),
            (ItemType::Soup, 45.0),
            (ItemType::Muffin, 30.0),
        ];

        for (item, hunger_boost) in &food_priority {
            if inv.has(*item, 1) {
                inv.remove(*item, 1);
                needs.hunger = (needs.hunger + hunger_boost).min(100.0);
                tracing::info!("[AUTO-EAT] {} auto-consumed {} (hunger was {:.0}, now {:.0})",
                    name.0, item, needs.hunger - hunger_boost, needs.hunger);
                event_log.push(crate::agents::event_log::LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: crate::agents::event_log::LogKind::System,
                    text: format!("Auto-consumed {} (hunger critical)", item),
                });
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_caps_high_values() {
        let mut n = Needs {
            energy: 150.0,
            hunger: 200.0,
            boredom: 999.0,
        };
        n.clamp();
        assert_eq!(n.energy, 100.0);
        assert_eq!(n.hunger, 100.0);
        assert_eq!(n.boredom, 100.0);
    }

    #[test]
    fn clamp_floors_negative_values() {
        let mut n = Needs {
            energy: -10.0,
            hunger: -0.5,
            boredom: -100.0,
        };
        n.clamp();
        assert_eq!(n.energy, 0.0);
        assert_eq!(n.hunger, 0.0);
        assert_eq!(n.boredom, 0.0);
    }

    #[test]
    fn clamp_preserves_valid_values() {
        let mut n = Needs {
            energy: 50.0,
            hunger: 0.0,
            boredom: 100.0,
        };
        n.clamp();
        assert_eq!(n.energy, 50.0);
        assert_eq!(n.hunger, 0.0);
        assert_eq!(n.boredom, 100.0);
    }

    #[test]
    fn most_urgent_none_when_all_above_threshold() {
        let n = Needs {
            energy: 80.0,
            hunger: 70.0,
            boredom: 60.0,
        };
        assert!(n.most_urgent(50.0).is_none());
    }

    #[test]
    fn most_urgent_returns_lowest_below_threshold() {
        let n = Needs {
            energy: 30.0,
            hunger: 10.0,
            boredom: 45.0,
        };
        assert_eq!(n.most_urgent(50.0), Some(NeedType::Hunger));
    }

    #[test]
    fn most_urgent_energy_wins_when_lowest() {
        let n = Needs {
            energy: 5.0,
            hunger: 20.0,
            boredom: 40.0,
        };
        assert_eq!(n.most_urgent(50.0), Some(NeedType::Energy));
    }

    #[test]
    fn most_urgent_boredom_wins_when_lowest() {
        let n = Needs {
            energy: 40.0,
            hunger: 35.0,
            boredom: 10.0,
        };
        assert_eq!(n.most_urgent(50.0), Some(NeedType::Boredom));
    }

    #[test]
    fn most_urgent_exactly_at_threshold_not_urgent() {
        // The check is strictly less-than, so exactly at threshold is not urgent
        let n = Needs {
            energy: 50.0,
            hunger: 50.0,
            boredom: 50.0,
        };
        assert!(n.most_urgent(50.0).is_none());
    }

    #[test]
    fn most_urgent_just_below_threshold() {
        let n = Needs {
            energy: 49.9,
            hunger: 80.0,
            boredom: 80.0,
        };
        assert_eq!(n.most_urgent(50.0), Some(NeedType::Energy));
    }

    #[test]
    fn default_values() {
        let n = Needs::default();
        assert_eq!(n.energy, 80.0);
        assert_eq!(n.hunger, 80.0);
        assert_eq!(n.boredom, 60.0);
    }
}
