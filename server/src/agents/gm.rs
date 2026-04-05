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

/// Marker component: this agent is waiting for a research result.
#[derive(Component)]
pub struct PendingResearch {
    pub topic: String,
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

/// System: spawn a research agent for agents using search_internet.
/// Produces a real document via Claude web search.
pub fn spawn_research_system(
    mut commands: Commands,
    runtime: ResMut<TokioTasksRuntime>,
    pending: Query<(Entity, &super::components::AgentName, &super::components::AgentId, &PendingResearch)>,
) {
    for (entity, agent_name, agent_id, research) in &pending {
        let name = agent_name.0.clone();
        let topic = research.topic.clone();
        let agent_uuid = agent_id.0.to_string();

        tracing::info!("[Research] Spawning research agent for {} — topic: {}", name, topic);
        commands.entity(entity).remove::<PendingResearch>();

        runtime.spawn_background_task(move |_ctx| async move {
            spawn_research_agent(&name, &agent_uuid, &topic).await;
        });
    }
}

async fn spawn_research_agent(agent_name: &str, agent_id: &str, topic: &str) {
    let prompt = format!(
r#"You are a research assistant. Do a brief web search on the following topic and write a short markdown research document (3-5 paragraphs).

Topic: {topic}

Write the document in markdown format. Keep it concise but factual. Include at least 2 key findings.
Output ONLY the markdown document, nothing else."#
    );

    tracing::info!("[Research] Launching claude -p for {} (agent: {})", topic, agent_name);

    let result = tokio::process::Command::new("claude")
        .args([
            "-p", &prompt,
            "--output-format", "text",
            "--model", "haiku",
            "--permission-mode", "bypassPermissions",
        ])
        .output()
        .await;

    let doc_content = match result {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            if text.trim().is_empty() {
                "Research completed but no content was produced.".to_string()
            } else {
                text
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("[Research] Process failed: {}", stderr.chars().take(200).collect::<String>());
            format!("Research failed: {}", stderr.chars().take(100).collect::<String>())
        }
        Err(e) => {
            tracing::error!("[Research] Failed to spawn: {}", e);
            format!("Research error: {}", e)
        }
    };

    let title = format!("research_{}.md", &agent_id[..6]);
    tracing::info!("[Research] {} produced document '{}' ({} chars)",
        agent_name, title, doc_content.len());

    // Post the document back to the game server.
    let client = reqwest::Client::new();
    let _ = client.post("http://127.0.0.1:8080/api/gm/document")
        .json(&serde_json::json!({
            "agent_name": agent_name,
            "title": title,
            "content": doc_content,
        }))
        .send()
        .await;
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

Be STRICT. The criteria say exactly what must be verified. Check inventories and action logs carefully.
If the criteria say "verify agent has document in inventory" and they don't, REJECT.
If the criteria say "verify agent visited X" and the logs don't show it, REJECT.
Do not give credit for "effort" — verify the actual evidence.

Do NOT communicate with agents. Just verify and submit your verdict."#
    );

    tracing::info!("[GM] Launching claude -p for bounty {} (agent: {})", bounty_id, agent_name);

    let result = tokio::process::Command::new("claude")
        .args([
            "-p", &prompt,
            "--output-format", "text",
            "--model", "sonnet",
            "--permission-mode", "bypassPermissions",
            "--mcp-config", &mcp_config_path,
        ])
        .output()
        .await;

    // Check if GM submitted a verdict by trying the endpoint.
    // If process fails or exits without submitting, auto-approve.
    async fn auto_approve(bounty_id: Uuid, reason: &str) {
        tracing::warn!("[GM] Auto-approving bounty {}: {}", bounty_id, reason);
        let client = reqwest::Client::new();
        let _ = client.post("http://127.0.0.1:8080/api/gm/verdict")
            .json(&serde_json::json!({
                "bounty_id": bounty_id.to_string(),
                "approved": true,
                "reason": format!("Auto-approved: {}", reason),
            }))
            .send()
            .await;
    }

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            tracing::info!("[GM] Verdict process exited for bounty {}. stdout: {}",
                bounty_id, stdout.chars().take(300).collect::<String>());

            // If exit code is non-zero or stdout contains an error, auto-approve.
            if !output.status.success() || stdout.contains("issue with") || stdout.contains("error") {
                auto_approve(bounty_id, "GM process failed or errored").await;
            }
            // If the GM didn't submit a verdict (no HTTP call made), the bounty
            // stays in PendingVerification. Check after a delay and auto-approve.
            // We'll do this with a simple sleep + check.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // Check if bounty is still pending — if so, auto-approve.
            let client = reqwest::Client::new();
            if let Ok(resp) = client.get(&format!("http://127.0.0.1:8080/api/gm/query?q=full")).send().await {
                if let Ok(body) = resp.text().await {
                    if body.contains(&format!("\"id\":\"{}\"", bounty_id)) && body.contains("PendingVerification") {
                        auto_approve(bounty_id, "GM did not submit verdict within timeout").await;
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("[GM] Failed to spawn claude for bounty {}: {}", bounty_id, e);
            auto_approve(bounty_id, "GM process failed to spawn").await;
        }
    }

    let _ = tokio::fs::remove_file(&mcp_config_path).await;
}
