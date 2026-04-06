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
use crate::llm::types::{SessionCommand, SessionEvent, SessionOwner, COMPACT_COMMAND};
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

    match provider_type.as_str() {
        "openai_responses" => {
            let system_prompt_clone = system_prompt.clone();
            runtime.spawn_background_task(move |mut ctx| async move {
                // Create bridged channels for gameplay interface.
                let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>(64);
                let (_evt_tx, mut evt_rx) = mpsc::channel::<SessionEvent>(256);

                let (prompt_tx, mut prompt_bridge_rx) = mpsc::channel::<String>(32);
                let cmd_tx_bridge = cmd_tx.clone();
                tokio::spawn(async move {
                    while let Some(text) = prompt_bridge_rx.recv().await {
                        if text == COMPACT_COMMAND {
                            let _ = cmd_tx_bridge.send(SessionCommand::Compact).await;
                        } else {
                            let _ = cmd_tx_bridge
                                .send(SessionCommand::SendUserTurn(text))
                                .await;
                        }
                    }
                });

                // Bridge events to token tracking.
                let (token_tx, token_rx) = mpsc::channel::<TokenUsageEvent>(64);
                let (gm_log_tx, gm_log_rx) =
                    mpsc::channel::<crate::network::system_relay::GmLogEntry>(64);
                tokio::spawn(async move {
                    while let Some(event) = evt_rx.recv().await {
                        match event {
                            SessionEvent::TextDelta(text) => {
                                let _ = gm_log_tx.try_send(
                                    crate::network::system_relay::GmLogEntry {
                                        kind: "response",
                                        text,
                                    },
                                );
                            }
                            SessionEvent::Usage(usage) => {
                                let _ = token_tx
                                    .send(TokenUsageEvent {
                                        input_tokens: usage.input_tokens,
                                        output_tokens: usage.output_tokens,
                                        cost_usd: usage.cost_usd,
                                    })
                                    .await;
                            }
                            SessionEvent::Error(msg) => {
                                tracing::warn!("[openai:system-ai] error: {}", msg);
                            }
                            _ => {}
                        }
                    }
                });

                // Start the OpenAI adapter via factory.
                let mut adapter =
                    crate::llm::supervisor::create_openai_system_ai_adapter(
                        &model,
                        system_prompt_clone,
                        tool_sets,
                    );

                let profile = crate::llm::config::SessionProfile {
                    provider: "openai".to_string(),
                    model: Some(model.clone()),
                    system_prompt_file: None,
                    compact_threshold: 15_000,
                    tool_sets: vec!["system".to_string()],
                };

                if let Err(e) = adapter.start(&profile, None).await {
                    tracing::error!("[openai:system-ai] failed to start: {}", e);
                    ctx.run_on_main_thread(|main_ctx| {
                        let world = main_ctx.world;
                        let mut system_ai = world.resource_mut::<SystemAiState>();
                        system_ai.spawning = false;
                    })
                    .await;
                    return;
                }

                let prompt_tx_for_main = prompt_tx.clone();
                let token_rx_for_main = Arc::new(Mutex::new(token_rx));
                let gm_log_rx_for_main = Arc::new(Mutex::new(gm_log_rx));

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
            });
        }

        // Default: claude_cli — uses relay + process spawning.
        _ => {
            runtime.spawn_background_task(move |mut ctx| async move {
                let handle = relays.register().await;

                // Spawn Claude process via the adapter factory.
                if let Err(e) =
                    crate::llm::supervisor::spawn_system_ai_process(
                        &model,
                        &system_prompt,
                        port,
                        &process_registry,
                    )
                    .await
                {
                    tracing::error!("[SystemAI] Failed to spawn: {}", e);
                    ctx.run_on_main_thread(|main_ctx| {
                        let world = main_ctx.world;
                        let mut system_ai = world.resource_mut::<SystemAiState>();
                        system_ai.spawning = false;
                    })
                    .await;
                    return;
                }

                // Wait for Claude to connect via WebSocket.
                let connected = handle.connected.clone();
                let connected_ok = tokio::select! {
                    _ = connected.notified() => true,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(90)) => false,
                };

                if !connected_ok {
                    tracing::error!("[SystemAI] Claude connection timeout (90s)");
                    ctx.run_on_main_thread(|main_ctx| {
                        let world = main_ctx.world;
                        let mut system_ai = world.resource_mut::<SystemAiState>();
                        system_ai.spawning = false;
                    })
                    .await;
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
            });
        }
    }
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
/// Provider selection flows through the `research` profile in llm.toml.
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

        // Resolve provider from research profile (one-shot, light model).
        let provider_type = llm_config
            .profile("research")
            .and_then(|p| llm_config.provider(&p.provider))
            .map(|p| p.provider_type.clone())
            .unwrap_or_else(|| "claude_cli".to_string());

        let model = llm_config
            .profile("research")
            .and_then(|p| llm_config.effective_model(p))
            .unwrap_or_else(|| "haiku".to_string());

        tracing::info!(
            "[Research] Spawning {} research agent for {} — topic: {} (model={})",
            provider_type,
            name,
            topic,
            model,
        );
        commands.entity(entity).remove::<PendingResearch>();

        let provider = provider_type.clone();
        let model_clone = model.clone();
        runtime.spawn_background_task(move |_ctx| async move {
            spawn_research_agent(&name, &agent_uuid, &topic, &provider, &model_clone)
                .await;
        });
    }
}

async fn spawn_research_agent(
    agent_name: &str,
    agent_id: &str,
    topic: &str,
    provider_type: &str,
    model: &str,
) {
    let prompt = format!(
        r#"You are a research assistant. Do a brief web search on the following topic and write a short markdown research document (3-5 paragraphs).

Topic: {topic}

Write the document in markdown format. Keep it concise but factual. Include at least 2 key findings.
Output ONLY the markdown document, nothing else."#
    );

    tracing::info!(
        "[Research] Launching {} research for {} (agent: {})",
        provider_type,
        topic,
        agent_name
    );

    let doc_content = match provider_type {
        "openai_responses" => run_openai_research(&prompt, model).await,
        _ => run_claude_research(&prompt, model).await,
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

/// Run research via Claude CLI one-shot (`claude -p`).
async fn run_claude_research(prompt: &str, model: &str) -> String {
    let result = tokio::process::Command::new("claude")
        .args([
            "-p",
            prompt,
            "--output-format",
            "text",
            "--model",
            model,
            "--permission-mode",
            "bypassPermissions",
        ])
        .output()
        .await;

    match result {
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
    }
}

/// Run research via OpenAI Responses API as a one-shot request.
async fn run_openai_research(prompt: &str, model: &str) -> String {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => return "Research failed: OPENAI_API_KEY not set".to_string(),
    };
    let api_base = std::env::var("OPENAI_API_BASE")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": model,
        "input": [{
            "role": "user",
            "content": prompt,
        }],
        "stream": false,
    });

    match client
        .post(format!("{api_base}/responses"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(val) => {
                    // Extract text from response output.
                    val.get("output")
                        .and_then(|o| o.as_array())
                        .and_then(|arr| {
                            arr.iter().find_map(|item| {
                                if item.get("type")?.as_str()? == "message" {
                                    item.get("content")?
                                        .as_array()?
                                        .iter()
                                        .find_map(|c| {
                                            if c.get("type")?.as_str()? == "output_text" {
                                                c.get("text")?.as_str().map(|s| s.to_string())
                                            } else {
                                                None
                                            }
                                        })
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or_else(|| {
                            "Research completed but no content was extracted.".to_string()
                        })
                }
                Err(e) => format!("Research failed: JSON parse error: {e}"),
            }
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "[Research:openai] API error {}: {}",
                status,
                &body[..body.len().min(200)]
            );
            format!("Research failed: API error {status}")
        }
        Err(e) => {
            tracing::error!("[Research:openai] HTTP error: {}", e);
            format!("Research error: {e}")
        }
    }
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
