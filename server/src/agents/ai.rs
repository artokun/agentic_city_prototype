use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::perception::KnownLocations;
use crate::agents::social::Relationships;
use crate::agents::token_tracking::TokenEventQueue;
use crate::items::Inventory;
use crate::llm::config::LlmConfig;
use crate::llm::providers::claude::AgentIdentity;
use crate::llm::session_registry::SessionRegistry;
use crate::llm::supervisor::SessionAdapter;
use crate::llm::types::{SessionCommand, SessionEvent, SessionOwner, COMPACT_COMMAND};
use crate::network::agent_relay::AgentRelays;
use crate::tick::TickCount;
use crate::world::bounty::{BountyBoard, BountyTokenStore};
use crate::world::map::GridPos;
use crate::world::shifts::ShiftWorker;
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

use super::actions::ActionTimer;
use super::personality::Personality;

/// Bevy resource: tracks sessions per agent.
/// Wraps the unified SessionRegistry with agent-specific state (decision tick, prompt).
#[derive(Resource, Default)]
pub struct AgentSessions {
    pub sessions: HashMap<Entity, SessionState>,
}

impl AgentSessions {
    /// Send a compaction command to the agent's session.
    /// Uses the unified SessionCommand::Compact via the registry channel.
    pub fn send_compact(&self, entity: &Entity) -> bool {
        if let Some(session) = self.sessions.get(entity) {
            session.prompt_tx.try_send(COMPACT_COMMAND.to_string()).is_ok()
        } else {
            false
        }
    }
}

pub struct SessionState {
    pub prompt_tx: mpsc::Sender<String>,
    pub response_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    pub last_decision_tick: u64,
    pub system_prompt: String,
}

/// Bevy resource wrapping AgentRelays for Axum.
#[derive(Resource, Clone)]
pub struct AgentRelaysResource(pub AgentRelays);

use crate::config;

/// System: spawn LLM sessions for agents via the unified session engine.
/// Reads each agent's `SessionProfileRef` to resolve provider and model from `LlmConfig`.
/// For `claude_cli` providers: spawns process via Claude adapter + relay.
/// For `openai_responses` providers: spawns self-contained streaming task.
/// Registers all sessions in `SessionRegistry` for lifecycle tracking.
pub fn spawn_sessions_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut sessions: ResMut<AgentSessions>,
    mut registry: ResMut<SessionRegistry>,
    relays: Res<AgentRelaysResource>,
    llm_config: Res<LlmConfig>,
    agents: Query<(
        Entity,
        &AgentId,
        &AgentName,
        &Personality,
        &ClaudeModel,
        &SessionProfileRef,
    )>,
    server_port: Option<Res<crate::network::ws::ServerPort>>,
    proc_registry: Option<Res<crate::process_manager::ProcessRegistryRes>>,
) {
    let port = server_port.map(|p| p.0).unwrap_or(8080);
    let process_registry = proc_registry.map(|r| r.0.clone()).unwrap_or_default();
    for (entity, agent_id, name, personality, claude_model, profile_ref) in &agents {
        if sessions.sessions.contains_key(&entity) {
            continue;
        }

        let agent_name = name.0.clone();
        let agent_uuid = agent_id.0.to_string();
        let system_prompt = super::personality::build_system_prompt(&name.0, personality);

        // Resolve provider type from profile. Fall back to claude_cli.
        let provider_type = llm_config
            .profile(&profile_ref.0)
            .and_then(|p| llm_config.provider(&p.provider))
            .map(|p| p.provider_type.as_str())
            .unwrap_or("claude_cli")
            .to_string();

        // Resolve effective model: profile override > component > provider default.
        let model_name = llm_config
            .profile(&profile_ref.0)
            .and_then(|p| llm_config.effective_model(p))
            .unwrap_or_else(|| claude_model.0.clone());

        // Resolve tool sets from profile.
        let tool_sets = llm_config
            .profile(&profile_ref.0)
            .map(|p| p.tool_sets.clone())
            .unwrap_or_else(|| vec!["game".to_string()]);

        tracing::info!(
            "Spawning {} session for {} ({}) [profile={}, model={}]",
            provider_type,
            agent_name,
            agent_uuid,
            profile_ref.0,
            model_name,
        );

        // Insert placeholder session — will be replaced with real channels on connect.
        let (placeholder_tx, _) = mpsc::channel(1);
        let (_, placeholder_rx) = mpsc::channel(1);
        sessions.sessions.insert(
            entity,
            SessionState {
                prompt_tx: placeholder_tx,
                response_rx: Arc::new(Mutex::new(placeholder_rx)),
                last_decision_tick: 0,
                system_prompt: system_prompt.clone(),
            },
        );

        // Register in SessionRegistry for lifecycle tracking.
        let owner = SessionOwner::Agent(agent_name.clone());
        let (reg_handle, _, _) =
            crate::llm::supervisor::create_handle_channels(&profile_ref.0);
        registry.register(owner, reg_handle);

        let entity_copy = entity;
        let process_registry = process_registry.clone();
        let sys_prompt_clone = system_prompt;
        let relays_clone = relays.0.clone();

        match provider_type.as_str() {
            "openai_responses" => {
                // OpenAI: self-contained adapter with HTTP SSE, no relay needed.
                let model = model_name.clone();
                let agent_name_clone = agent_name.clone();
                let agent_uuid_clone = agent_uuid.clone();

                runtime.spawn_background_task(move |mut ctx| async move {
                    // Create command/event channels for the gameplay interface.
                    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
                    let (evt_tx, evt_rx) = mpsc::channel::<SessionEvent>(256);

                    // Create a bridge: prompt_tx (String) -> cmd_tx (SessionCommand).
                    let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(32);
                    let cmd_tx_bridge = cmd_tx.clone();
                    tokio::spawn(async move {
                        while let Some(text) = prompt_rx.recv().await {
                            if text == COMPACT_COMMAND {
                                let _ = cmd_tx_bridge.send(SessionCommand::Compact).await;
                            } else {
                                let _ = cmd_tx_bridge
                                    .send(SessionCommand::SendUserTurn(text))
                                    .await;
                            }
                        }
                    });

                    // Create a bridge: evt_rx (SessionEvent) -> response_tx (String) + token_tx.
                    let (response_tx, response_rx) = mpsc::channel::<String>(64);
                    let (token_tx, token_rx) =
                        mpsc::channel::<crate::network::agent_relay::TokenUsageEvent>(64);
                    let agent_label = agent_name_clone.clone();
                    tokio::spawn(async move {
                        let mut evt_rx = evt_rx;
                        while let Some(event) = evt_rx.recv().await {
                            match event {
                                SessionEvent::TextDelta(text) => {
                                    let msg = format!("thought:{}", text);
                                    let _ = response_tx.send(msg).await;
                                }
                                SessionEvent::Usage(usage) => {
                                    let _ = token_tx
                                        .send(
                                            crate::network::agent_relay::TokenUsageEvent {
                                                input_tokens: usage.input_tokens,
                                                output_tokens: usage.output_tokens,
                                                cost_usd: usage.cost_usd,
                                            },
                                        )
                                        .await;
                                }
                                SessionEvent::Error(msg) => {
                                    tracing::warn!(
                                        "[openai:{}] session error: {}",
                                        agent_label,
                                        msg
                                    );
                                }
                                _ => {}
                            }
                        }
                    });

                    // Start the OpenAI adapter streaming task.
                    let mut adapter =
                        crate::llm::providers::openai::OpenAiAdapter::for_agent(
                            &agent_name_clone,
                            &agent_uuid_clone,
                            &model,
                            sys_prompt_clone.clone(),
                            tool_sets,
                        );

                    let profile = crate::llm::config::SessionProfile {
                        provider: "openai".to_string(),
                        model: Some(model.clone()),
                        system_prompt_file: None,
                        compact_threshold: 50_000,
                        tool_sets: vec!["game".to_string()],
                    };

                    if let Err(e) = adapter.start(&profile, None).await {
                        tracing::error!(
                            "[openai:{}] failed to start: {}",
                            agent_name_clone,
                            e
                        );
                        ctx.run_on_main_thread(move |main_ctx| {
                            let world = main_ctx.world;
                            let mut sessions =
                                world.resource_mut::<AgentSessions>();
                            sessions.sessions.remove(&entity_copy);
                        })
                        .await;
                        return;
                    }

                    // Send intro message via command channel.
                    let intro = build_intro_message(&agent_name_clone);
                    let _ = cmd_tx.send(SessionCommand::SendUserTurn(intro)).await;

                    let ptx_clone = prompt_tx.clone();
                    let rrx_clone = Arc::new(Mutex::new(response_rx));
                    let token_rx_clone = Arc::new(Mutex::new(token_rx));
                    let uuid_for_main = agent_uuid_clone.clone();

                    ctx.run_on_main_thread(move |main_ctx| {
                        let world = main_ctx.world;
                        let mut sessions =
                            world.resource_mut::<AgentSessions>();
                        if let Some(s) = sessions.sessions.get_mut(&entity_copy) {
                            s.prompt_tx = ptx_clone;
                            s.response_rx = rrx_clone;
                            tracing::info!(
                                "[openai] Session channels updated for {:?}",
                                entity_copy
                            );
                        }
                        let mut token_queue =
                            world.resource_mut::<TokenEventQueue>();
                        token_queue
                            .receivers
                            .insert(uuid_for_main, token_rx_clone);
                    })
                    .await;
                });
            }

            // Default: claude_cli — uses relay + process spawning.
            _ => {
                let model_name = model_name.clone();
                runtime.spawn_background_task(move |mut ctx| async move {
                    // Register relay for this agent.
                    let handle = relays_clone.register(&agent_uuid).await;

                    // Spawn Claude process via the adapter.
                    let identity = AgentIdentity {
                        name: agent_name.clone(),
                        uuid: agent_uuid.clone(),
                    };
                    if let Err(e) =
                        crate::llm::providers::claude::spawn_agent_process(
                            &identity,
                            &model_name,
                            &sys_prompt_clone,
                            port,
                            &process_registry,
                        )
                        .await
                    {
                        tracing::error!(
                            "Failed to spawn claude for {}: {}",
                            agent_name,
                            e
                        );
                        ctx.run_on_main_thread(move |main_ctx| {
                            let world = main_ctx.world;
                            let mut sessions =
                                world.resource_mut::<AgentSessions>();
                            sessions.sessions.remove(&entity_copy);
                        })
                        .await;
                        return;
                    }

                    // Wait for Claude to connect via WebSocket.
                    tracing::info!(
                        "Waiting for Claude to connect for {}...",
                        agent_name
                    );
                    let connected = handle.connected.clone();
                    tokio::select! {
                        _ = connected.notified() => {
                            tracing::info!("Claude connected via --sdk-url for {}", agent_name);
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(90)) => {
                            tracing::error!("Claude connection timeout for {} (90s)", agent_name);
                            ctx.run_on_main_thread(move |main_ctx| {
                                let world = main_ctx.world;
                                let mut sessions = world.resource_mut::<AgentSessions>();
                                sessions.sessions.remove(&entity_copy);
                                tracing::info!("Cleared session placeholder for {} — will retry", agent_name);
                            }).await;
                            return;
                        }
                    }

                    // Wire relay channels into the session state.
                    let ptx = handle.prompt_tx;
                    let rrx = Arc::new(Mutex::new(handle.response_rx));
                    let token_rx = Arc::new(Mutex::new(handle.token_rx));

                    // Send the intro message.
                    let intro = build_intro_message(&agent_name);
                    let _ = ptx.send(intro).await;

                    let ptx_clone = ptx.clone();
                    let rrx_clone = rrx.clone();
                    let token_rx_clone = token_rx.clone();
                    let agent_uuid_for_main = agent_uuid.clone();

                    ctx.run_on_main_thread(move |main_ctx| {
                        let world = main_ctx.world;
                        let mut sessions =
                            world.resource_mut::<AgentSessions>();
                        if let Some(s) = sessions.sessions.get_mut(&entity_copy) {
                            s.prompt_tx = ptx_clone;
                            s.response_rx = rrx_clone;
                            tracing::info!(
                                "Session channels updated for entity {:?}",
                                entity_copy
                            );
                        }
                        let mut token_queue =
                            world.resource_mut::<TokenEventQueue>();
                        token_queue
                            .receivers
                            .insert(agent_uuid_for_main, token_rx_clone);
                    })
                    .await;
                });
            }
        }
    }
}

/// System: drain agent thoughts from relay and log them.
/// These are Claude's reasoning text BEFORE tool calls — visible in the activity log.
pub fn ai_thought_drain_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut sessions: ResMut<AgentSessions>,
    mut agents: Query<(Entity, &AgentName, &mut ThoughtBubble)>,
) {
    for (entity, name, mut thought) in &mut agents {
        let Some(session) = sessions.sessions.get_mut(&entity) else {
            continue;
        };

        // Drain all available messages from the relay.
        loop {
            let msg = {
                let mut rx = session.response_rx.lock().unwrap();
                match rx.try_recv() {
                    Ok(text) => {
                        let preview: String = text.chars().take(60).collect();
                        tracing::info!("[drain:{}] received: {}...", name.0, preview);
                        Some(text)
                    }
                    Err(_) => None,
                }
            };
            let Some(text) = msg else { break };

            if let Some(thought_text) = text.strip_prefix("thought:") {
                thought.0 = thought_text.to_string();

                event_log.push(LogEvent {
                    tick: tick.0,
                    agent: name.0.clone(),
                    kind: LogKind::Thought,
                    text: thought_text.to_string(),
                });
            }
            // Non-thought messages (result text) are ignored — actions come via MCP tool.
        }
    }
}

/// System: send context updates to LLM sessions.
/// The LLM acts via the MCP game_action tool — we do NOT parse responses.
/// This system only provides situational awareness.
pub fn ai_context_system(
    tick: Res<TickCount>,
    boards_ai: Query<&BountyTokenStore, With<BountyBoard>>,
    mut sessions: ResMut<AgentSessions>,
    agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &Speed,
        &AgentGoal,
        &Needs,
        &Inventory,
        &KnownLocations,
        &Relationships,
        Option<&ActionTimer>,
        Option<&Path>,
        Option<&InsideBuilding>,
        Option<&ShiftWorker>,
        &crate::items::CarrySlots,
        &BusinessCards,
    )>,
    all_agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    let Some(bounty_registry) = boards_ai.iter().next() else {
        return;
    };
    for (
        entity,
        name,
        pos,
        speed,
        goal,
        needs,
        inv,
        known_locs,
        rels,
        action_timer,
        path,
        inside_building,
        shift_worker,
        carry_slots,
        business_cards,
    ) in &agents
    {
        if action_timer.is_some() {
            continue;
        }

        // Don't prompt incapacitated agents — they're passed out.
        if matches!(goal, AgentGoal::Idle) && needs.energy <= 0.0 {
            continue;
        }
        let has_path = path.is_some_and(|p| !p.0.is_empty());
        if has_path {
            continue;
        }

        let Some(session) = sessions.sessions.get_mut(&entity) else {
            continue;
        };

        // Rate limit context updates.
        if tick.0 - session.last_decision_tick < config::context_interval() {
            continue;
        }

        // Only send context when agent can act.
        if matches!(goal, AgentGoal::PerformingAction) {
            continue;
        }

        // Bounty visibility: MUST be physically at the bounty board to see bounties.
        let at_board = known_locs
            .locations
            .values()
            .find(|l| l.name == "bounty_board")
            .is_some_and(|l| pos.x == l.entrance.x && pos.y == l.entrance.y);

        let available_bounties: Vec<String> = bounty_registry
            .tokens
            .values()
            .filter(|b| {
                let is_mine = b.claimed_by == Some(entity);
                if is_mine {
                    return true;
                }
                at_board && b.state == crate::world::bounty::BountyState::Available
            })
            .map(|b| {
                let is_mine = b.claimed_by == Some(entity);
                let status = match b.state {
                    crate::world::bounty::BountyState::Claimed if is_mine => "(YOUR ACTIVE BOUNTY)",
                    crate::world::bounty::BountyState::PendingVerification if is_mine => {
                        "(PENDING GM REVIEW)"
                    }
                    crate::world::bounty::BountyState::Claimed => "(taken)",
                    _ => "(available)",
                };
                let instructions = if is_mine {
                    let agent_instructions = b
                        .hidden_criteria
                        .split("\n\nGM:")
                        .next()
                        .unwrap_or("")
                        .strip_prefix("Instructions for agent: ")
                        .unwrap_or("");
                    if agent_instructions.is_empty() {
                        String::new()
                    } else {
                        format!("\n  HOW TO COMPLETE: {}", agent_instructions)
                    }
                } else {
                    String::new()
                };
                let short_id = &b.id.to_string()[..6];
                format!(
                    "[{}] {} — {}g {}{}",
                    short_id, b.description, b.reward_gold, status, instructions
                )
            })
            .collect();

        let nearby: Vec<(String, GridPos)> = all_agents
            .iter()
            .filter(|(n, _)| n.0 != name.0)
            .filter(|(_, p)| (p.x - pos.x).abs() + (p.y - pos.y).abs() <= 10)
            .map(|(n, p)| (n.0.clone(), *p))
            .collect();

        // Location-specific tools.
        let mut location_tools: Vec<&str> = vec![
            "look_around",
            "wander",
            "go_to_board",
            "go_to_service",
            "work_shift",
        ];
        if let Some(inside) = inside_building {
            if let Ok((_, _, sprite)) = structures.get(inside.0) {
                let on_shift = shift_worker.is_some();
                match sprite.0.as_str() {
                    "google" => {
                        location_tools.push("search_internet");
                    }
                    "cafe" if on_shift => {
                        location_tools.push("brew_coffee");
                        location_tools.push("sell_to_customer");
                    }
                    "market" if on_shift => {
                        location_tools.push("stock_shelves");
                        location_tools.push("sell_to_customer");
                    }
                    "warehouse" if on_shift => {
                        location_tools.push("buy_wholesale");
                    }
                    "hotel" if on_shift => {
                        location_tools.push("check_in_guest");
                    }
                    "apartments" => {
                        location_tools.push("cook_meal");
                        location_tools.push("rest");
                    }
                    "library" => {
                        location_tools.push("search_library — search documents by keyword");
                        location_tools.push(
                            "copy_document — copy a document to your inventory (service=title)",
                        );
                    }
                    "bounty_board" => {
                        location_tools.push("claim_bounty");
                        location_tools.push("redeem_paycheck");
                    }
                    _ => {}
                }
            }
        }
        if matches!(goal, AgentGoal::InteractingWithBoard) {
            if !location_tools.contains(&"claim_bounty") {
                location_tools.push("claim_bounty");
            }
            if !location_tools.contains(&"redeem_paycheck") {
                location_tools.push("redeem_paycheck");
            }
        }
        if shift_worker.is_some() {
            location_tools.push("leave_shift");
        }
        if matches!(goal, AgentGoal::ExecutingBounty(_)) {
            location_tools.push("complete_bounty");
        }

        let active_bounty_desc = match goal {
            AgentGoal::ExecutingBounty(bid) => {
                bounty_registry.get(*bid).map(|b| b.description.clone())
            }
            _ => None,
        };

        let context = super::ai_decision::build_context(
            &name.0,
            pos,
            needs,
            inv,
            goal,
            known_locs,
            rels,
            speed.0,
            &available_bounties,
            &nearby,
            &location_tools,
            active_bounty_desc.as_deref(),
            carry_slots,
            business_cards,
        );

        // Send context as a user message. The LLM will think and call game_action tool.
        if let Err(e) = session.prompt_tx.try_send(context) {
            tracing::debug!("[AI:{}] context send failed: {}", name.0, e);
            continue;
        }

        session.last_decision_tick = tick.0;
    }
}

/// Build the intro message sent to agents when they first spawn.
fn build_intro_message(agent_name: &str) -> String {
    let others = match agent_name {
        "Alice" | "Alice Haiku" => "Bob and Carol",
        "Bob" | "Bob Sonnet" => "Alice and Carol",
        "Carol" | "Carol Opus" => "Alice and Bob",
        _ => "the other contestants",
    };

    format!(
        r#"ATTENTION, {agent_name}.

You have been summoned to San Francisco.

You stand on a cold sidewalk in a city you've never seen before. The fog rolls in from the bay. You have nothing — no gold, no food, no contacts, and no idea what's coming. The world is watching.

You are not alone. {others} have also been summoned. They are your competitors — and potentially your allies. You will all compete to earn as much gold as possible. Who you trust, who you trade with, and who you undercut is entirely up to you.

HERE ARE THE RULES:

1. GOLD IS EVERYTHING. The contestant who earns the most gold wins. Gold comes from completing bounties posted at the bounty board and from working shifts at local businesses. Bounties pay 4-15 gold and are the fastest path to riches. Shifts pay slowly but reliably.

2. YOU HAVE NEEDS. Your energy, hunger, and boredom are constantly draining. If any drops to zero, you will collapse and wake up at the hospital with a 5 gold debt. Keep your needs above critical levels — but don't waste time over-maintaining them either. Every tick spent eating is a tick not earning.

3. THE CITY IS YOUR PLAYGROUND. You can visit buildings: the bounty board, cafes, a hotel, apartments, a warehouse, a market, a library, a theater, and more. Each offers services — some free, some costly. Learn which ones are worth your time.

4. YOU INTERACT THROUGH ACTIONS. Use your game_action tool. That is your ONLY way to affect the world. Move, eat, sleep, work, claim bounties, chat — everything goes through that tool.

5. SOCIAL CONNECTIONS MATTER. You can start conversations with nearby agents, exchange business cards, send messages, and even trade items. Chatting relieves boredom for free. But don't waste time on small talk when there's gold to earn.

6. THE SYSTEM IS WATCHING. Every bounty you submit is verified by the System — an all-seeing AI that will approve or reject your work. The System is... opinionated. Don't take it personally. If something feels broken, use the "help" action.

You have been placed near the bounty board. That is not a coincidence. Your first move matters.

Good luck, {agent_name}. The clock is ticking.
"#
    )
}
