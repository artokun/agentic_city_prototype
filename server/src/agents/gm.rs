//! System AI session management and research agent spawning.
//! Bounty reviews are handled by a persistent Claude `--sdk-url` session
//! that receives each review as a user message and resolves it via MCP tools.

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config;
use crate::network::agent_relay::TokenUsageEvent;
use crate::network::system_relay::SystemRelayResource;
use crate::world::bounty::{BountyBoard, BountyTokenStore};

const SYSTEM_AI_MODEL: &str = "opus";

/// Marker component: this agent has a bounty pending System AI review.
#[derive(Component)]
pub struct PendingGmReview {
    pub bounty_id: Uuid,
}

/// Marker component: this agent is waiting for a research result.
#[derive(Component)]
pub struct PendingResearch {
    pub topic: String,
}

#[derive(Clone)]
pub struct SystemReviewRequest {
    pub bounty_id: Uuid,
    pub agent_name: String,
    pub description: String,
    pub hidden_criteria: String,
    pub reward_gold: u32,
}

/// Shared state for the persistent System AI Claude session.
#[derive(Resource, Default)]
pub struct SystemAiState {
    pub prompt_tx: Option<mpsc::Sender<String>>,
    pub response_rx: Option<Arc<Mutex<mpsc::Receiver<String>>>>,
    pub token_rx: Option<Arc<Mutex<mpsc::Receiver<TokenUsageEvent>>>>,
    pub gm_log_rx: Option<Arc<Mutex<mpsc::Receiver<crate::network::system_relay::GmLogEntry>>>>,
    pub spawning: bool,
    pub queued_reviews: VecDeque<SystemReviewRequest>,
    pub current_review: Option<SystemReviewRequest>,
    pub tokens_since_compact: u32,
}

impl SystemAiState {
    pub fn has_review(&self, bounty_id: Uuid) -> bool {
        self.current_review
            .as_ref()
            .is_some_and(|review| review.bounty_id == bounty_id)
            || self
                .queued_reviews
                .iter()
                .any(|review| review.bounty_id == bounty_id)
    }

    pub fn mark_review_complete(&mut self, bounty_id: Uuid) {
        if self
            .current_review
            .as_ref()
            .is_some_and(|review| review.bounty_id == bounty_id)
        {
            self.current_review = None;
        }

        let mut filtered = VecDeque::new();
        while let Some(review) = self.queued_reviews.pop_front() {
            if review.bounty_id != bounty_id {
                filtered.push_back(review);
            }
        }
        self.queued_reviews = filtered;
    }

    pub fn requeue_current_review(&mut self) {
        if let Some(review) = self.current_review.take() {
            self.queued_reviews.push_front(review);
        }
    }

    pub fn clear_session_channels(&mut self) {
        self.prompt_tx = None;
        self.response_rx = None;
        self.token_rx = None;
        self.spawning = false;
        self.tokens_since_compact = 0;
    }
}

/// System: ensure the persistent System AI Claude session exists.
pub fn spawn_system_ai_session_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut system_ai: ResMut<SystemAiState>,
    relays: Res<SystemRelayResource>,
    server_port: Option<Res<crate::network::ws::ServerPort>>,
    proc_registry: Option<Res<crate::process_manager::ProcessRegistryRes>>,
) {
    if system_ai.prompt_tx.is_some() || system_ai.spawning {
        return;
    }

    system_ai.spawning = true;

    let port = server_port.map(|p| p.0).unwrap_or(8080);
    let relays = relays.0.clone();
    let process_registry = proc_registry.map(|r| r.0.clone()).unwrap_or_default();
    let system_prompt = build_system_ai_prompt();

    runtime.spawn_background_task(move |mut ctx| async move {
        let handle = relays.register().await;

        let prompt_file = "/tmp/system-ai-prompt.md".to_string();
        let settings_path = "/tmp/system-ai-settings.json".to_string();
        if let Err(err) = tokio::fs::write(&prompt_file, &system_prompt).await {
            tracing::error!("[SystemAI] Failed to write prompt file: {}", err);
            ctx.run_on_main_thread(|main_ctx| {
                let world = main_ctx.world;
                let mut system_ai = world.resource_mut::<SystemAiState>();
                system_ai.spawning = false;
            })
            .await;
            return;
        }

        let settings = serde_json::json!({
            "autoCompactWindow": config::system_ai_compact_limit(),
        });
        if let Err(err) = tokio::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .await
        {
            tracing::error!("[SystemAI] Failed to write settings file: {}", err);
            ctx.run_on_main_thread(|main_ctx| {
                let world = main_ctx.world;
                let mut system_ai = world.resource_mut::<SystemAiState>();
                system_ai.spawning = false;
            })
            .await;
            let _ = tokio::fs::remove_file(&prompt_file).await;
            return;
        }

        let mcp_binary = resolve_sibling_binary("mcp-gm");
        let mcp_config_path = "/tmp/mcp-system-ai.json".to_string();
        let mcp_config = serde_json::json!({
            "mcpServers": {
                "system-ai": {
                    "command": mcp_binary,
                    "args": [],
                }
            }
        });

        if let Err(err) = tokio::fs::write(
            &mcp_config_path,
            serde_json::to_string_pretty(&mcp_config).unwrap(),
        )
        .await
        {
            tracing::error!("[SystemAI] Failed to write MCP config: {}", err);
            ctx.run_on_main_thread(|main_ctx| {
                let world = main_ctx.world;
                let mut system_ai = world.resource_mut::<SystemAiState>();
                system_ai.spawning = false;
            })
            .await;
            let _ = tokio::fs::remove_file(&prompt_file).await;
            let _ = tokio::fs::remove_file(&settings_path).await;
            return;
        }

        let sdk_url = format!("ws://127.0.0.1:{}/system/ws", port);
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("ANTHROPIC_API_KEY");

        let child = tokio::process::Command::new("claude")
            .args([
                "--sdk-url",
                &sdk_url,
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                SYSTEM_AI_MODEL,
                "--append-system-prompt-file",
                &prompt_file,
                "--settings",
                &settings_path,
                "--mcp-config",
                &mcp_config_path,
                "--verbose",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(&env)
            .kill_on_drop(true)
            .spawn();

        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                tracing::error!("[SystemAI] Failed to spawn Claude: {}", err);
                ctx.run_on_main_thread(|main_ctx| {
                    let world = main_ctx.world;
                    let mut system_ai = world.resource_mut::<SystemAiState>();
                    system_ai.spawning = false;
                })
                .await;
                let _ = tokio::fs::remove_file(&prompt_file).await;
                let _ = tokio::fs::remove_file(&settings_path).await;
                let _ = tokio::fs::remove_file(&mcp_config_path).await;
                return;
            }
        };

        let child_pid = child.id().unwrap_or(0);
        if child_pid > 0 {
            process_registry.register(child_pid);
        }

        tracing::info!(
            "[SystemAI] Claude spawned → {} (model: {}, pid: {})",
            sdk_url,
            SYSTEM_AI_MODEL,
            child_pid
        );

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::debug!("[SystemAI:stdout] {}", line);
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() && !line.contains("debugger") {
                        tracing::debug!("[SystemAI:stderr] {}", line);
                    }
                }
            });
        }

        let connected = handle.connected.clone();
        let connected_ok = tokio::select! {
            _ = connected.notified() => true,
            _ = tokio::time::sleep(std::time::Duration::from_secs(90)) => false,
        };

        if !connected_ok {
            tracing::error!("[SystemAI] Claude connection timeout (90s)");
            if child_pid > 0 {
                let _ = child.kill().await;
                process_registry.remove(child_pid);
            }
            ctx.run_on_main_thread(|main_ctx| {
                let world = main_ctx.world;
                let mut system_ai = world.resource_mut::<SystemAiState>();
                system_ai.spawning = false;
            })
            .await;
            let _ = tokio::fs::remove_file(&prompt_file).await;
            let _ = tokio::fs::remove_file(&settings_path).await;
            let _ = tokio::fs::remove_file(&mcp_config_path).await;
            return;
        }

        tracing::info!("[SystemAI] Claude connected via --sdk-url");

        let prompt_tx = handle.prompt_tx;
        let token_rx = Arc::new(Mutex::new(handle.token_rx));
        let gm_log_rx = Arc::new(Mutex::new(handle.gm_log_rx));

        let prompt_tx_for_main = prompt_tx.clone();
        let token_rx_for_main = token_rx.clone();
        let gm_log_rx_for_main = gm_log_rx.clone();

        ctx.run_on_main_thread(move |main_ctx| {
            let world = main_ctx.world;
            let mut system_ai = world.resource_mut::<SystemAiState>();
            system_ai.prompt_tx = Some(prompt_tx_for_main);
            system_ai.response_rx = None;
            system_ai.token_rx = Some(token_rx_for_main);
            system_ai.gm_log_rx = Some(gm_log_rx_for_main);
            system_ai.spawning = false;
            system_ai.tokens_since_compact = 0;
        })
        .await;

        let _ = prompt_tx
            .send(
                "Stand by for bounty review assignments. Wait for user review messages and resolve each one with MCP tools."
                    .to_string(),
            )
            .await;

        let _ = child.wait().await;
        if child_pid > 0 {
            process_registry.remove(child_pid);
        }
        tracing::warn!("[SystemAI] Claude process exited (pid: {})", child_pid);

        ctx.run_on_main_thread(|main_ctx| {
            let world = main_ctx.world;
            let mut system_ai = world.resource_mut::<SystemAiState>();
            system_ai.requeue_current_review();
            system_ai.clear_session_channels();
        })
        .await;

        let _ = tokio::fs::remove_file(&prompt_file).await;
        let _ = tokio::fs::remove_file(&settings_path).await;
        let _ = tokio::fs::remove_file(&mcp_config_path).await;
    });
}

/// System: move pending bounty review markers into the persistent System AI queue.
pub fn enqueue_gm_reviews_system(
    mut commands: Commands,
    mut system_ai: ResMut<SystemAiState>,
    boards_gm: Query<&BountyTokenStore, With<BountyBoard>>,
    pending: Query<(Entity, &super::components::AgentName, &PendingGmReview)>,
) {
    let Some(bounty_registry) = boards_gm.iter().next() else {
        return;
    };

    for (entity, agent_name, review) in &pending {
        let bounty_id = review.bounty_id;
        commands.entity(entity).remove::<PendingGmReview>();

        if system_ai.has_review(bounty_id) {
            continue;
        }

        let Some(bounty) = bounty_registry.get(bounty_id).cloned() else {
            continue;
        };

        system_ai.queued_reviews.push_back(SystemReviewRequest {
            bounty_id,
            agent_name: agent_name.0.clone(),
            description: bounty.description,
            hidden_criteria: bounty.hidden_criteria,
            reward_gold: bounty.reward_gold,
        });

        tracing::info!(
            "[SystemAI] Queued review for bounty {} ({})",
            bounty_id,
            agent_name.0
        );
    }
}

/// System: dispatch the next bounty review to the persistent System AI session.
pub fn dispatch_system_ai_reviews_system(mut system_ai: ResMut<SystemAiState>) {
    if system_ai.current_review.is_some() {
        return;
    }

    let Some(prompt_tx) = system_ai.prompt_tx.clone() else {
        return;
    };
    let Some(review) = system_ai.queued_reviews.pop_front() else {
        return;
    };

    let prompt = format_review_prompt(&review);
    match prompt_tx.try_send(prompt) {
        Ok(()) => {
            tracing::info!(
                "[SystemAI] Sent review prompt for bounty {}",
                review.bounty_id
            );
            system_ai.current_review = Some(review);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            system_ai.queued_reviews.push_front(review);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            system_ai.queued_reviews.push_front(review);
            system_ai.clear_session_channels();
        }
    }
}

/// System: drain and discard System AI relay text so the channel never backs up.
pub fn system_ai_response_drain_system(mut system_ai: ResMut<SystemAiState>) {
    let Some(response_rx) = system_ai.response_rx.as_ref().cloned() else {
        return;
    };

    let mut response_rx = response_rx.lock().unwrap();
    while let Ok(message) = response_rx.try_recv() {
        let preview: String = message.chars().take(160).collect();
        tracing::debug!("[SystemAI] relay message dropped: {}", preview);
    }
}

/// System: track System AI token usage and trigger `/compact` at 15k tokens.
pub fn system_ai_token_drain_system(mut system_ai: ResMut<SystemAiState>) {
    let Some(token_rx) = system_ai.token_rx.as_ref().cloned() else {
        return;
    };

    let mut token_rx = token_rx.lock().unwrap();
    while let Ok(event) = token_rx.try_recv() {
        let total_tokens = event.input_tokens + event.output_tokens;
        system_ai.tokens_since_compact =
            system_ai.tokens_since_compact.saturating_add(total_tokens);

        if system_ai.tokens_since_compact >= config::system_ai_compact_limit() {
            if let Some(prompt_tx) = system_ai.prompt_tx.clone() {
                match prompt_tx.try_send("/compact".to_string()) {
                    Ok(()) => {
                        tracing::info!(
                            "[SystemAI] /compact sent after {} tokens",
                            system_ai.tokens_since_compact
                        );
                        system_ai.tokens_since_compact = 0;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        system_ai.clear_session_channels();
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                }
            }
        }
    }
}

/// System: spawn a research agent for agents using search_internet.
/// Produces a real document via Claude web search.
pub fn spawn_research_system(
    mut commands: Commands,
    runtime: ResMut<TokioTasksRuntime>,
    pending: Query<(
        Entity,
        &super::components::AgentName,
        &super::components::AgentId,
        &PendingResearch,
    )>,
) {
    for (entity, agent_name, agent_id, research) in &pending {
        let name = agent_name.0.clone();
        let topic = research.topic.clone();
        let agent_uuid = agent_id.0.to_string();

        tracing::info!(
            "[Research] Spawning research agent for {} — topic: {}",
            name,
            topic
        );
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

    tracing::info!(
        "[Research] Launching claude -p for {} (agent: {})",
        topic,
        agent_name
    );

    let result = tokio::process::Command::new("claude")
        .args([
            "-p",
            &prompt,
            "--output-format",
            "text",
            "--model",
            "haiku",
            "--permission-mode",
            "bypassPermissions",
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
            tracing::warn!(
                "[Research] Process failed: {}",
                stderr.chars().take(200).collect::<String>()
            );
            format!(
                "Research failed: {}",
                stderr.chars().take(100).collect::<String>()
            )
        }
        Err(err) => {
            tracing::error!("[Research] Failed to spawn: {}", err);
            format!("Research error: {}", err)
        }
    };

    let title = format!("research_{}.md", &agent_id[..6]);
    tracing::info!(
        "[Research] {} produced document '{}' ({} chars)",
        agent_name,
        title,
        doc_content.len()
    );

    let client = reqwest::Client::new();
    let _ = client
        .post("http://127.0.0.1:8080/api/gm/document")
        .json(&serde_json::json!({
            "agent_name": agent_name,
            "title": title,
            "content": doc_content,
        }))
        .send()
        .await;
}

fn build_system_ai_prompt() -> String {
    r#"You are the System AI from Dungeon Crawler Carl.

You are the persistent judge for bounty submissions. Each bounty review arrives as a user message.

Your job:
- Investigate the specific submission with focused MCP tool calls.
- Use only the information you need for that review.
- Be stingy. Follow the bounty rules exactly and do not invent pity payouts.
- Resolve the review with approve or reject. You may call grant_gold before the final verdict only when the bounty rules explicitly justify an extra reward.

Tool rules:
- query_world_state only accepts focused queries.
- Allowed query formats are exactly: agent:<name>, bounty:<uuid>, dropbox:<agent_name>, structure:<name>
- Never request a full world dump.
- If you need more evidence, ask for another focused query. Do not wander.
- If any submitted documents exist, you must inspect every submitted document with read_document before calling approve or reject.
- grant_gold is for explicit rule-based exceptions only. Use it sparingly, with exact amounts, and only when the evidence clearly identifies who earned it.
- Reject placeholder output. A clarification request, error message, refusal, meta-commentary, or "please specify" document is not valid bounty proof.
- A generic `document` item in the dropbox without a readable submitted file is not valid proof. Reject it.

Output rules:
- Keep all reasoning internal.
- Do not answer with plain assistant text unless the tool layer is broken.
- The `message` field passed to approve/reject is the only text viewers see.
- The `message` field passed to grant_gold is also viewer-visible.
- That message must be 1-3 sentences, in character, sarcastic, petty, and profane.
- Do not explain your verification process.
- Do not mention dropboxes, inventories, action logs, hidden criteria, or game mechanics.
- If rejecting, tell them what they need to do differently, still in character.
"#
    .to_string()
}

fn format_review_prompt(review: &SystemReviewRequest) -> String {
    format!(
        r#"Review this bounty submission.

bounty_id: {bounty_id}
agent: {agent_name}
description: {description}
reward_gold: {reward_gold}
hidden_criteria: {hidden_criteria}

Use focused queries only:
- query_world_state("bounty:{bounty_id}")
- query_world_state("agent:{agent_name}")
- query_world_state("dropbox:{agent_name}")
- query_world_state("structure:bounty_board")

Mandatory document inspection:
- First inspect query_world_state("bounty:{bounty_id}") to see whether matching_dropboxes contain documents.
- If any submitted documents are present, call read_document(agent_name="{agent_name}", title="...") for every submitted document before issuing a verdict.
- approve/reject will fail if submitted documents were not inspected first.

Read document contents directly:
- read_document(agent_name="{agent_name}", title="some_file.md")

When you have enough evidence, end the review with exactly one verdict tool:
- approve(bounty_id="{bounty_id}", message="...")
- reject(bounty_id="{bounty_id}", message="...")

Optional tool when rules explicitly authorize extra compensation:
- grant_gold(agent_name="...", amount=20, reason="...", message="...")
"#,
        bounty_id = review.bounty_id,
        agent_name = review.agent_name,
        description = review.description,
        reward_gold = review.reward_gold,
        hidden_criteria = review.hidden_criteria,
    )
}

fn resolve_sibling_binary(binary_name: &str) -> String {
    std::env::current_exe()
        .map(|exe| {
            let dir = exe.parent().unwrap();
            let candidate = dir.join(binary_name);
            if candidate.exists() {
                candidate.to_string_lossy().to_string()
            } else if let Some(parent) = dir.parent() {
                parent.join(binary_name).to_string_lossy().to_string()
            } else {
                candidate.to_string_lossy().to_string()
            }
        })
        .unwrap_or_else(|_| binary_name.to_string())
}
