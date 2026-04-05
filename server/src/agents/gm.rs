//! Game Master agent spawning.
//! When a bounty is submitted for verification, this system spawns a one-shot
//! Claude Opus agent via `claude -p` that queries the world state, evaluates
//! the hidden acceptance criteria, and submits a verdict.

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use uuid::Uuid;

use crate::world::bounty::BountyRegistry;

/// Marker component: this agent has a bounty pending GM review.
#[derive(Component)]
pub struct PendingGmReview {
    pub bounty_id: Uuid,
}

/// System: spawn a one-shot game master Claude agent for each pending review.
pub fn spawn_gm_system(
    mut commands: Commands,
    runtime: ResMut<TokioTasksRuntime>,
    bounty_registry: Res<BountyRegistry>,
    pending: Query<(Entity, &super::components::AgentName, &PendingGmReview)>,
) {
    for (entity, agent_name, review) in &pending {
        let bounty = bounty_registry.get(review.bounty_id).cloned();
        let Some(bounty) = bounty else {
            commands.entity(entity).remove::<PendingGmReview>();
            continue;
        };

        let bounty_id = review.bounty_id;
        let agent = agent_name.0.clone();
        let description = bounty.description.clone();
        let hidden_criteria = bounty.hidden_criteria.clone();
        let reward = bounty.reward_gold;

        tracing::info!("[GM] Spawning game master for bounty '{}' (agent: {})", description, agent);

        // Remove marker immediately — GM is spawned.
        commands.entity(entity).remove::<PendingGmReview>();

        // Spawn the GM process in a background task.
        runtime.spawn_background_task(move |_ctx| async move {
            spawn_gm_agent(bounty_id, &agent, &description, &hidden_criteria, reward).await;
        });
    }
}

async fn spawn_gm_agent(
    bounty_id: Uuid,
    agent_name: &str,
    description: &str,
    hidden_criteria: &str,
    reward: u32,
) {
    let mcp_binary = std::env::current_exe()
        .map(|exe| exe.parent().unwrap().join("mcp-gm").to_string_lossy().to_string())
        .unwrap_or_else(|_| "mcp-gm".into());

    let mcp_config_path = format!("/tmp/mcp-gm-{}.json", bounty_id);
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "game-master": {
                "command": mcp_binary,
                "args": [],
            }
        }
    });
    let _ = std::fs::write(&mcp_config_path, serde_json::to_string_pretty(&mcp_config).unwrap());

    let prompt = format!(
r#"You are a GAME MASTER verifying a bounty completion. You must be fair but strict.

BOUNTY DETAILS:
- Title: {description}
- Reward: {reward} gold
- Submitted by: {agent_name}
- Bounty ID: {bounty_id}

ACCEPTANCE CRITERIA (hidden from agents):
{hidden_criteria}

YOUR TASK:
1. Use query_world_state to check the current game state
2. Evaluate whether the agent actually completed the bounty based on the criteria
3. Use submit_verdict with bounty_id="{bounty_id}" to approve or reject

Be FAIR but STRICT. If the agent clearly made an effort and the criteria are loosely met, approve.
If there's no evidence of completion, reject with a clear explanation.

Do NOT communicate with agents. Just verify and submit your verdict."#
    );

    tracing::info!("[GM] Launching claude -p for bounty {} (agent: {})", bounty_id, agent_name);

    let result = tokio::process::Command::new("claude")
        .args([
            "-p", &prompt,
            "--output-format", "text",
            "--model", "claude-sonnet-4-5-20250514",
            "--permission-mode", "bypassPermissions",
            "--mcp-config", &mcp_config_path,
        ])
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::info!("[GM] Verdict process exited for bounty {}. stdout: {}",
                bounty_id, stdout.chars().take(200).collect::<String>());
            if !stderr.is_empty() {
                tracing::debug!("[GM] stderr: {}", stderr.chars().take(200).collect::<String>());
            }
        }
        Err(e) => {
            tracing::error!("[GM] Failed to spawn claude for bounty {}: {}", bounty_id, e);
            // Auto-approve on GM failure to avoid blocking the game.
            let client = reqwest::Client::new();
            let _ = client.post("http://127.0.0.1:8080/api/gm/verdict")
                .json(&serde_json::json!({
                    "bounty_id": bounty_id.to_string(),
                    "approved": true,
                    "reason": "Auto-approved: GM process failed to spawn",
                }))
                .send()
                .await;
        }
    }

    let _ = tokio::fs::remove_file(&mcp_config_path).await;
}
