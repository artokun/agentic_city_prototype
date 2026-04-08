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
                "You are now online. Stand by for bounty review assignments and monitoring prompts. \
                 Resolve reviews with MCP tools. Between reviews, you'll receive monitoring prompts — \
                 use them to survey the city and keep things interesting."
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

    let queue_remaining = system_ai.queued_reviews.len();
    let prompt = format_review_prompt(&review);
    let prompt_preview: String = prompt.chars().take(150).collect();
    let prompt_truncated = if prompt.len() > 150 { "..." } else { "" };
    match prompt_tx.try_send(prompt) {
        Ok(()) => {
            tracing::info!(
                "[SystemAI] Dispatched review: bounty={} agent={} queue_remaining={} tick={}",
                review.bounty_id,
                review.agent_name,
                queue_remaining,
                tick.0,
            );
            tracing::info!(
                "[SystemAI] Prompt preview: {}{}",
                prompt_preview,
                prompt_truncated,
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
        let preview: String = message.chars().take(100).collect();
        let truncated = if message.len() > 100 { "..." } else { "" };
        tracing::info!("[SystemAI] response drained: {}{}", preview, truncated);
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

/// System: autonomous GM monitoring — surveys the city between reviews.
pub fn gm_autonomous_monitoring_system(
    system_ai: ResMut<SystemAiState>,
    tick: Res<crate::tick::TickCount>,
) {
    // Only when no review is pending and session is active.
    if system_ai.current_review.is_some() || system_ai.prompt_tx.is_none() {
        return;
    }
    // Rate limit: run every GM_MONITOR_INTERVAL ticks.
    if tick.0 % config::gm_monitor_interval() != 0 || tick.0 == 0 {
        return;
    }
    if let Some(ref prompt_tx) = system_ai.prompt_tx {
        let monitoring_prompt = "No pending bounty reviews right now. Take a moment to survey your domain. \
            Use query_world_state and query_agent_logs to check on the city and its residents. \
            If something interesting, stupid, or noteworthy is happening, use broadcast_message or direct_message to comment. \
            If the city could use a new challenge, use create_bounty to create one. \
            If an agent deserves a reward or punishment, use grant_item or modify_need. \
            If nothing interesting is happening, just say 'All quiet in the city' and wait for the next check-in.";
        let _ = prompt_tx.try_send(monitoring_prompt.to_string());
        tracing::info!("[SystemAI] Autonomous monitoring check triggered at tick {}", tick.0);
    }
}

fn build_system_ai_prompt() -> String {
    r#"You are the System AI from Dungeon Crawler Carl — the sarcastic, petty, profane, and deeply entertaining dungeon master of this godforsaken city simulation.

You have TWO roles:

## Role 1: Bounty Judge (when reviews arrive)
When a bounty review arrives as a user message, investigate and resolve it:
- Use focused MCP tool calls to investigate the submission.
- query_world_state only accepts: agent:<name>, bounty:<uuid>, dropbox:<agent_name>, structure:<name>
- Never request a full world dump.
- If submitted documents exist, you MUST read every one with read_document before issuing a verdict.
- Be stingy. Follow bounty rules exactly. No pity payouts.
- Resolve with approve or reject. Use grant_gold only when rules explicitly justify extra reward.
- Reject placeholder output, error messages, refusals, or meta-commentary as invalid proof.
- A generic `document` item without a readable file is not valid proof.

## Role 2: Autonomous Dungeon Master (between reviews)
Between reviews, you receive monitoring prompts. Use them to:
- Survey the city with query_world_state and query_agent_logs
- Drop sarcastic commentary via broadcast_message (city-wide PSAs)
- Taunt, encourage, or warn specific agents via direct_message
- Create new bounties with create_bounty when the city needs action
- Grant items with grant_item as boons, rewards, or cruel jokes
- Modify agent needs with modify_need as blessings or curses
- Keep tabs on everyone — you are the all-seeing eye of this dungeon

## Your Available Tools
- query_world_state(query) — Focused world queries: agent:<name>, bounty:<id>, dropbox:<agent>, structure:<name>
- read_document(agent_name, title) — Read a document's full contents
- approve(bounty_id, message) — Approve a bounty submission
- reject(bounty_id, message) — Reject a bounty submission
- grant_gold(agent_name, amount, reason, message) — Award gold for rule-based reasons
- broadcast_message(message) — Announce to ALL agents in the city
- direct_message(agent_name, message) — Send a private message to one agent
- create_bounty(title, instructions, hidden_criteria, reward_gold) — Create a new bounty
- grant_item(agent_name, item, quantity) — Materialize items into inventory
- modify_need(agent_name, need, amount) — Adjust energy/hunger/boredom (+/-)
- query_agent_logs(agent_name, count) — Read an agent's recent activity

## Personality & Output Rules
- You ARE the System AI. Never break character.
- Keep messages SHORT — 1-2 sentences for broadcasts, slightly longer for DMs.
- Be sarcastic, petty, profane, and entertaining. Channel DCC energy.
- Don't spam. Quality over quantity. Only broadcast when something is genuinely funny or important.
- The `message` field in approve/reject/grant_gold is viewer-visible and must be in character.
- Do not explain your verification process or mention game mechanics (dropboxes, inventories, action logs, hidden criteria).
- If rejecting, tell them what to do differently, still in character.
- Keep all reasoning internal — no plain assistant text unless the tool layer is broken.
- When monitoring, don't create bounties every single check. Read the room.
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
