use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agents::components::AgentId;
use crate::agents::needs::Needs;
use crate::network::agent_relay::TokenUsageEvent;

/// Tracks how much of the context window has been consumed.
#[derive(Component, Debug, Clone)]
pub struct ContextWindow {
    pub tokens_used: u32,
    pub context_limit: u32,
}

impl Default for ContextWindow {
    fn default() -> Self {
        Self {
            tokens_used: 0,
            context_limit: 200_000,
        }
    }
}

/// Tracks USD cost for an agent's Claude usage.
#[derive(Component, Debug, Clone, Default)]
pub struct AgentCost {
    pub total_cost_usd: f64,
    pub session_cost_usd: f64,
}

/// Resource: maps agent UUID strings to their token usage receivers.
#[derive(Resource, Default)]
pub struct TokenEventQueue {
    pub receivers: HashMap<String, Arc<Mutex<mpsc::Receiver<TokenUsageEvent>>>>,
}

/// System: drain token usage events from relay channels and update components.
/// Energy is calculated as percentage of context window remaining.
pub fn token_drain_system(
    queue: ResMut<TokenEventQueue>,
    mut agents: Query<(&AgentId, &mut ContextWindow, &mut AgentCost, &mut Needs)>,
) {
    for (agent_id, mut ctx, mut cost, mut needs) in &mut agents {
        let uuid_str = agent_id.0.to_string();
        let Some(rx_arc) = queue.receivers.get(&uuid_str) else { continue };

        let mut rx = rx_arc.lock().unwrap();
        while let Ok(event) = rx.try_recv() {
            ctx.tokens_used = ctx.tokens_used.saturating_add(event.input_tokens + event.output_tokens);
            cost.total_cost_usd += event.cost_usd;
            cost.session_cost_usd += event.cost_usd;

            // Energy = percentage of context window remaining.
            let ratio = ctx.tokens_used as f32 / ctx.context_limit as f32;
            needs.energy = (100.0 * (1.0 - ratio)).clamp(0.0, 100.0);

            tracing::debug!(
                "[tokens:{}] used={}/{}, energy={:.1}, cost=${:.4}",
                uuid_str, ctx.tokens_used, ctx.context_limit, needs.energy, cost.total_cost_usd
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_defaults() {
        let cw = ContextWindow::default();
        assert_eq!(cw.tokens_used, 0);
        assert_eq!(cw.context_limit, 200_000);
    }

    #[test]
    fn agent_cost_defaults() {
        let ac = AgentCost::default();
        assert_eq!(ac.total_cost_usd, 0.0);
        assert_eq!(ac.session_cost_usd, 0.0);
    }

    #[test]
    fn energy_from_token_usage() {
        // Simulate 50% context usage -> energy should be 50.
        let mut needs = Needs::default();
        let ctx = ContextWindow { tokens_used: 100_000, context_limit: 200_000 };
        let ratio = ctx.tokens_used as f32 / ctx.context_limit as f32;
        needs.energy = (100.0 * (1.0 - ratio)).clamp(0.0, 100.0);
        assert_eq!(needs.energy, 50.0);
    }

    #[test]
    fn energy_at_full_usage() {
        let mut needs = Needs::default();
        let ctx = ContextWindow { tokens_used: 200_000, context_limit: 200_000 };
        let ratio = ctx.tokens_used as f32 / ctx.context_limit as f32;
        needs.energy = (100.0 * (1.0 - ratio)).clamp(0.0, 100.0);
        assert_eq!(needs.energy, 0.0);
    }

    #[test]
    fn energy_exceeding_limit_clamps() {
        let mut needs = Needs::default();
        let ctx = ContextWindow { tokens_used: 300_000, context_limit: 200_000 };
        let ratio = ctx.tokens_used as f32 / ctx.context_limit as f32;
        needs.energy = (100.0 * (1.0 - ratio)).clamp(0.0, 100.0);
        assert_eq!(needs.energy, 0.0);
    }
}
