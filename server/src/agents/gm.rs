//! System AI session management and research agent spawning.
//! Bounty reviews are handled by a persistent LLM session that receives
//! each review as a user message and resolves it via MCP tools.
//!
//! Provider selection is driven by the `system-ai` profile in `config/llm.toml`.
//! For `claude_cli`: spawns process via Claude adapter + relay.
//! For `openai_responses`: uses self-contained streaming task.
//! This module is role-focused: it manages review queues, dispatching, and
//! token tracking — not provider-specific details.

use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config;
use crate::llm::config::LlmConfig;
use crate::llm::session_registry::SessionRegistry;
use crate::llm::types::{SessionOwner, COMPACT_COMMAND};
use crate::network::agent_relay::TokenUsageEvent;
use crate::network::system_relay::SystemRelayResource;
use crate::world::bounty::{BountyBoard, BountyTokenStore};

/// Default model when no profile is configured.
const DEFAULT_SYSTEM_AI_MODEL: &str = "opus";

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

/// Shared state for the persistent System AI session.
#[derive(Resource, Default)]
pub struct SystemAiState {
    pub prompt_tx: Option<mpsc::Sender<String>>,
    pub response_rx: Option<Arc<Mutex<mpsc::Receiver<String>>>>,
    pub token_rx: Option<Arc<Mutex<mpsc::Receiver<TokenUsageEvent>>>>,
    pub gm_log_rx: Option<Arc<Mutex<mpsc::Receiver<crate::network::system_relay::GmLogEntry>>>>,
    pub spawning: bool,
    pub queued_reviews: VecDeque<SystemReviewRequest>,
    pub current_review: Option<SystemReviewRequest>,
    pub current_review_dispatched_at: Option<u64>,
    pub tokens_since_compact: u32,
    /// Backoff state for failed GM session spawns.
    pub spawn_backoff: super::ai::SpawnBackoff,
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
            self.current_review_dispatched_at = None;
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
        // Requeue any in-flight review so it isn't lost when the session restarts.
        self.requeue_current_review();
        self.current_review_dispatched_at = None;
    }

    /// Send a compaction command to the System AI session.
    pub fn send_compact(&self) -> bool {
        if let Some(ref prompt_tx) = self.prompt_tx {
            prompt_tx.try_send(COMPACT_COMMAND.to_string()).is_ok()
        } else {
            false
        }
    }
}

/// System: ensure the persistent System AI session exists.
/// Resolves provider from the `system-ai` profile in `config/llm.toml`.
/// For `claude_cli`: spawns process via Claude adapter + relay.
/// For `openai_responses`: uses self-contained streaming task.
/// Registers in `SessionRegistry` for lifecycle tracking.
pub fn spawn_system_ai_session_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut system_ai: ResMut<SystemAiState>,
    mut registry: ResMut<SessionRegistry>,
    relays: Res<SystemRelayResource>,
    llm_config: Res<LlmConfig>,
    tick: Res<crate::tick::TickCount>,
    server_port: Option<Res<crate::network::ws::ServerPort>>,
    proc_registry: Option<Res<crate::process_manager::ProcessRegistryRes>>,
) {
    if system_ai.prompt_tx.is_some() || system_ai.spawning {
        return;
    }
    // Skip if no profiles configured (e.g. test harness with empty LlmConfig).
    if llm_config.profiles.is_empty() {
        return;
    }
    // Check backoff from prior spawn failures.
    // The GM is essential — without it, no bounties can be verified.
    if system_ai.spawn_backoff.gave_up() {
        panic!(
            "[SystemAI] FATAL: Game Master failed to spawn after {} attempts. \
             The game cannot function without the GM. Check LLM provider configuration.",
            system_ai.spawn_backoff.failures
        );
    }
    if !system_ai.spawn_backoff.should_retry(tick.0) {
        return;
    }

    system_ai.spawning = true;

    let port = server_port.map(|p| p.0).unwrap_or(8080);
    let relays = relays.0.clone();
    let process_registry = proc_registry.map(|r| r.0.clone()).unwrap_or_default();
    let system_prompt = build_system_ai_prompt();

    // Resolve provider type and model from profile.
    let provider_type = llm_config
        .profile("system-ai")
        .and_then(|p| llm_config.provider(&p.provider))
        .map(|p| p.provider_type.clone())
        .unwrap_or_else(|| "claude_cli".to_string());

    let model = llm_config
        .profile("system-ai")
        .and_then(|p| llm_config.effective_model(p))
        .unwrap_or_else(|| DEFAULT_SYSTEM_AI_MODEL.to_string());

    let tool_sets = llm_config
        .profile("system-ai")
        .map(|p| p.tool_sets.clone())
        .unwrap_or_else(|| vec!["system".to_string()]);

    tracing::info!(
        "[SystemAI] Spawning via {} (model={}, tools={:?})",
        provider_type,
        model,
        tool_sets,
    );

    // Register in SessionRegistry for lifecycle tracking.
    let owner = SessionOwner::SystemAi;
    let (reg_handle, _, _) =
        crate::llm::supervisor::create_handle_channels("system-ai");
    registry.register(owner, reg_handle);

    // Unified spawn path — the factory handles provider routing.
    let llm_config_clone = llm_config.clone();
    runtime.spawn_background_task(move |mut ctx| async move {
        let spawn_params = crate::llm::supervisor::SpawnParams {
            profile_name: "system-ai".to_string(),
            system_prompt: system_prompt.clone(),
            agent_identity: None,
            ws_port: port,
            process_registry: process_registry.clone(),
            agent_relays: None,
            system_relay: Some(relays.clone()),
        };

        let bridge = match crate::llm::supervisor::spawn_session(
            &llm_config_clone, spawn_params,
        ).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("[SystemAI] Failed to spawn: {}", e);
                ctx.run_on_main_thread(|main_ctx| {
                    let world = main_ctx.world;
                    let current_tick = world.resource::<crate::tick::TickCount>().0;
                    let mut system_ai = world.resource_mut::<SystemAiState>();
                    system_ai.spawning = false;
                    system_ai.spawn_backoff.record_failure(current_tick);
                    if system_ai.spawn_backoff.gave_up() {
                        tracing::error!(
                            "[SystemAI] Giving up after {} failures",
                            system_ai.spawn_backoff.failures
                        );
                    } else {
                        tracing::warn!(
                            "[SystemAI] Will retry after tick {} (attempt {}/{})",
                            system_ai.spawn_backoff.retry_after_tick,
                            system_ai.spawn_backoff.failures,
                            super::ai::SpawnBackoff::MAX_RETRIES
                        );
                    }
                }).await;
                return;
            }
        };

        // Wait for provider to be ready (watch channel: true = connected).
        let mut connected_rx = bridge.connected.clone();
        if !crate::agents::ai::wait_for_connected(&mut connected_rx, "SystemAI", 90).await {
            ctx.run_on_main_thread(|main_ctx| {
                let world = main_ctx.world;
                let current_tick = world.resource::<crate::tick::TickCount>().0;
                let mut system_ai = world.resource_mut::<SystemAiState>();
                system_ai.spawning = false;
                system_ai.spawn_backoff.record_failure(current_tick);
                if system_ai.spawn_backoff.gave_up() {
                    tracing::error!(
                        "[SystemAI] Giving up after {} connection failures",
                        system_ai.spawn_backoff.failures
                    );
                } else {
                    tracing::warn!(
                        "[SystemAI] Connection timeout, will retry after tick {} (attempt {}/{})",
                        system_ai.spawn_backoff.retry_after_tick,
                        system_ai.spawn_backoff.failures,
                        super::ai::SpawnBackoff::MAX_RETRIES
                    );
                }
            }).await;
            return;
        }

        // Spawn succeeded — clear any backoff state.
        ctx.run_on_main_thread(|main_ctx| {
            let world = main_ctx.world;
            let mut system_ai = world.resource_mut::<SystemAiState>();
            if system_ai.spawn_backoff.failures > 0 {
                tracing::info!(
                    "[SystemAI] Connected after {} prior failures",
                    system_ai.spawn_backoff.failures
                );
            }
            system_ai.spawn_backoff.reset();
        }).await;

        let prompt_tx_for_main = bridge.prompt_tx.clone();
        let token_rx_for_main = Arc::new(Mutex::new(bridge.token_rx));
        let gm_log_rx_for_main = bridge.gm_log_rx.map(|rx| Arc::new(Mutex::new(rx)));

        ctx.run_on_main_thread(move |main_ctx| {
            let world = main_ctx.world;
            let mut system_ai = world.resource_mut::<SystemAiState>();
            system_ai.prompt_tx = Some(prompt_tx_for_main);
            system_ai.response_rx = None;
            system_ai.token_rx = Some(token_rx_for_main);
            system_ai.gm_log_rx = gm_log_rx_for_main;
            system_ai.spawning = false;
            system_ai.tokens_since_compact = 0;
        }).await;

        let _ = bridge.prompt_tx
            .send(
                "Stand by for bounty review assignments. Wait for user review messages and resolve each one with MCP tools."
                    .to_string(),
            )
            .await;
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
/// If a review has been pending longer than `gm_review_timeout_ticks`, requeue it.
pub fn dispatch_system_ai_reviews_system(
    mut system_ai: ResMut<SystemAiState>,
    tick: Res<crate::tick::TickCount>,
) {
    // Check for stuck reviews — if the current review has been pending too long,
    // requeue it so the pipeline doesn't block forever.
    if let (Some(review), Some(dispatched_at)) = (
        system_ai.current_review.as_ref(),
        system_ai.current_review_dispatched_at,
    ) {
        let elapsed = tick.0.saturating_sub(dispatched_at);
        let timeout = config::gm_review_timeout_ticks();
        if elapsed >= timeout {
            tracing::warn!(
                "[SystemAI] Review for bounty {} stuck for {} ticks (timeout={}), requeuing",
                review.bounty_id,
                elapsed,
                timeout,
            );
            system_ai.requeue_current_review();
            system_ai.current_review_dispatched_at = None;
        } else {
            return;
        }
    } else if system_ai.current_review.is_some() {
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
                "[SystemAI] Sent review prompt for bounty {} (tick={})",
                review.bounty_id,
                tick.0,
            );
            system_ai.current_review = Some(review);
            system_ai.current_review_dispatched_at = Some(tick.0);
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
pub fn system_ai_response_drain_system(system_ai: ResMut<SystemAiState>) {
    let Some(response_rx) = system_ai.response_rx.as_ref().cloned() else {
        return;
    };

    let mut response_rx = response_rx.lock().unwrap();
    while let Ok(message) = response_rx.try_recv() {
        let preview: String = message.chars().take(160).collect();
        tracing::debug!("[SystemAI] relay message dropped: {}", preview);
    }
}

/// System: track System AI token usage and trigger compaction.
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
            if system_ai.send_compact() {
                tracing::info!(
                    "[SystemAI] compaction sent after {} tokens",
                    system_ai.tokens_since_compact
                );
                system_ai.tokens_since_compact = 0;
            } else if system_ai.prompt_tx.is_none() {
                system_ai.clear_session_channels();
            }
        }
    }
}

/// System: spawn a research agent for agents using search_internet.
/// Produces a real document via the configured LLM provider.
/// Provider selection flows through the `research` profile in llm.toml
/// via the unified `run_oneshot()` factory.
pub fn spawn_research_system(
    mut commands: Commands,
    runtime: ResMut<TokioTasksRuntime>,
    llm_config: Res<LlmConfig>,
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
        let config = llm_config.clone();

        tracing::debug!(
            "[Research] Spawning research agent for {} — topic: {}",
            name,
            topic,
        );
        commands.entity(entity).remove::<PendingResearch>();

        runtime.spawn_background_task(move |_ctx| async move {
            spawn_research_agent(&name, &agent_uuid, &topic, &config).await;
        });
    }
}

async fn spawn_research_agent(
    agent_name: &str,
    agent_id: &str,
    topic: &str,
    config: &LlmConfig,
) {
    let prompt = format!(
        r#"You are a research assistant. Do a brief web search on the following topic and write a short markdown research document (3-5 paragraphs).

Topic: {topic}

Write the document in markdown format. Keep it concise but factual. Include at least 2 key findings.
Output ONLY the markdown document, nothing else."#
    );

    tracing::debug!(
        "[Research] Launching research for {} (agent: {})",
        topic,
        agent_name
    );

    let doc_content = match crate::llm::supervisor::run_oneshot(config, "research", &prompt).await {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!("[Research] Failed for {}: {}", agent_name, e);
            format!("Research failed: {e}")
        }
    };

    let title = format!("research_{}.md", &agent_id[..6]);
    tracing::debug!(
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
- If you need more evidence, ask for another focused query. Do not ask for irrelevant or broad queries.
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
