use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::perception::KnownLocations;
use crate::agents::token_tracking::TokenEventQueue;
use crate::items::Inventory;
use crate::llm::config::LlmConfig;
use crate::llm::session_registry::SessionRegistry;
use crate::llm::types::{AgentIdentity, SessionOwner, COMPACT_COMMAND};
use crate::network::agent_relay::AgentRelays;
use crate::tick::TickCount;
use crate::world::bounty::{BountyBoard, BountyTokenStore};
use crate::world::map::GridPos;
use crate::world::shifts::ShiftWorker;
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

use super::actions::ActionTimer;
use super::personality::Personality;

fn build_location_tools(
    inside_building_name: Option<&str>,
    interacting_with_board: bool,
    on_shift: bool,
    executing_bounty: bool,
) -> Vec<&'static str> {
    let mut location_tools: Vec<&'static str> = vec!["look_around", "check_known_locations"];

    if !on_shift {
        location_tools.push("work_shift");
    }

    match inside_building_name {
        Some("cafe") => {
            location_tools.push("buy_muffin");
            location_tools.push("buy_coffee");
            location_tools.push("hang_out");
        }
        Some("market") => {
            location_tools.push("buy_sandwich");
            location_tools.push("buy_rations");
            location_tools.push("window_shop");
        }
        Some("hotel") => {
            location_tools.push("sleep_hotel");
            location_tools.push("relax_in_lobby");
        }
        Some("google") => {
            location_tools.push("search_internet");
        }
        Some("apartments") => {
            location_tools.push("sleep_at_home");
        }
        Some("library") => {
            location_tools.push("read_library");
            location_tools.push("inspect");
            location_tools.push("copy_document");
        }
        Some("bounty_board") => {
            location_tools.push("claim_bounty");
            location_tools.push("redeem_paycheck");
        }
        _ => {}
    }

    if interacting_with_board {
        if !location_tools.contains(&"claim_bounty") {
            location_tools.push("claim_bounty");
        }
        let redeem_hint = "redeem_paycheck";
        if !location_tools.contains(&redeem_hint) {
            location_tools.push(redeem_hint);
        }
    }

    if on_shift {
        location_tools.push("leave_shift");
    }

    if executing_bounty {
        location_tools.push("complete_bounty");
    }

    location_tools
}

/// Tracks exponential backoff state for failed spawn attempts.
#[derive(Default)]
pub struct SpawnBackoff {
    /// Number of consecutive failures.
    pub failures: u32,
    /// Tick at which the next spawn attempt is allowed.
    pub retry_after_tick: u64,
}

impl SpawnBackoff {
    /// Maximum number of retries before giving up (logs once and stops).
    pub const MAX_RETRIES: u32 = 8;
    /// Base delay in ticks (doubles each failure: 5, 10, 20, 40, ...).
    pub const BASE_DELAY_TICKS: u64 = 5;
    /// Absolute cap on delay (5 minutes at 1 tick/sec).
    pub const MAX_DELAY_TICKS: u64 = 300;

    pub fn record_failure(&mut self, current_tick: u64) {
        self.failures += 1;
        let delay = (Self::BASE_DELAY_TICKS * (1u64 << self.failures.min(10)))
            .min(Self::MAX_DELAY_TICKS);
        self.retry_after_tick = current_tick + delay;
    }

    pub fn should_retry(&self, current_tick: u64) -> bool {
        self.failures < Self::MAX_RETRIES && current_tick >= self.retry_after_tick
    }

    pub fn gave_up(&self) -> bool {
        self.failures >= Self::MAX_RETRIES
    }

    pub fn reset(&mut self) {
        self.failures = 0;
        self.retry_after_tick = 0;
    }
}

/// Bevy resource: tracks sessions per agent.
/// Wraps the unified SessionRegistry with agent-specific state (decision tick, prompt).
#[derive(Resource, Default)]
pub struct AgentSessions {
    pub sessions: HashMap<Entity, SessionState>,
    /// Tracks backoff state for agents whose sessions failed to spawn.
    /// Keyed by agent name (not Entity) to avoid stale state on entity recycling.
    pub spawn_backoff: HashMap<String, SpawnBackoff>,
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
    /// Stored for potential checkpoint/resume use.
    #[allow(dead_code)]
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
    tick: Res<TickCount>,
    agents: Query<(
        Entity,
        &AgentId,
        &AgentName,
        &Personality,
        &SessionProfileRef,
    )>,
    server_port: Option<Res<crate::network::ws::ServerPort>>,
    proc_registry: Option<Res<crate::process_manager::ProcessRegistryRes>>,
) {
    // Skip if no profiles configured (e.g. test harness with empty LlmConfig).
    if llm_config.profiles.is_empty() {
        return;
    }
    let current_tick = tick.0;
    let port = server_port.map(|p| p.0).unwrap_or(8080);
    let process_registry = proc_registry.map(|r| r.0.clone()).unwrap_or_default();
    for (entity, agent_id, name, personality, profile_ref) in &agents {
        if sessions.sessions.contains_key(&entity) {
            continue;
        }

        let agent_name = name.0.clone();

        // Check backoff: skip if we're still in a cooldown period after a failure.
        if let Some(backoff) = sessions.spawn_backoff.get(&agent_name) {
            if backoff.gave_up() {
                continue; // Permanently gave up after MAX_RETRIES
            }
            if !backoff.should_retry(current_tick) {
                continue; // Still in backoff window
            }
        }
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
            .unwrap_or_else(|| "haiku".to_string());

        // Resolve tool sets from profile.
        let _tool_sets = llm_config
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
        let llm_config_clone = llm_config.clone();

        // Unified spawn path — the factory handles provider routing.
        let profile_name = profile_ref.0.clone();
        runtime.spawn_background_task(move |mut ctx| async move {
            let spawn_params = crate::llm::supervisor::SpawnParams {
                profile_name: profile_name.clone(),
                system_prompt: sys_prompt_clone.clone(),
                agent_identity: Some(AgentIdentity {
                    name: agent_name.clone(),
                    uuid: agent_uuid.clone(),
                }),
                ws_port: port,
                process_registry: process_registry.clone(),
                agent_relays: Some(relays_clone),
                system_relay: None,
            };

            let mut bridge = match crate::llm::supervisor::spawn_session(
                &llm_config_clone, spawn_params,
            ).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("Failed to spawn session for {}: {}", agent_name, e);
                    let agent_name_for_backoff = agent_name.clone();
                    ctx.run_on_main_thread(move |main_ctx| {
                        let world = main_ctx.world;
                        let current_tick = world.resource::<TickCount>().0;
                        let mut sessions = world.resource_mut::<AgentSessions>();
                        sessions.sessions.remove(&entity_copy);
                        let backoff = sessions.spawn_backoff
                            .entry(agent_name_for_backoff)
                            .or_default();
                        backoff.record_failure(current_tick);
                        if backoff.gave_up() {
                            tracing::error!(
                                "Giving up on spawning session for {} after {} failures",
                                agent_name, backoff.failures
                            );
                        } else {
                            tracing::warn!(
                                "Will retry spawning {} after tick {} (attempt {}/{})",
                                agent_name, backoff.retry_after_tick,
                                backoff.failures, SpawnBackoff::MAX_RETRIES
                            );
                        }
                    }).await;
                    return;
                }
            };

            // Wait for provider to be ready (watch channel: true = connected).
            if !wait_for_connected(&mut bridge.connected, &agent_name, 90).await {
                let agent_name_for_backoff = agent_name.clone();
                ctx.run_on_main_thread(move |main_ctx| {
                    let world = main_ctx.world;
                    let current_tick = world.resource::<TickCount>().0;
                    let mut sessions = world.resource_mut::<AgentSessions>();
                    sessions.sessions.remove(&entity_copy);
                    let backoff = sessions.spawn_backoff
                        .entry(agent_name_for_backoff.clone())
                        .or_default();
                    backoff.record_failure(current_tick);
                    tracing::warn!(
                        "Session for {} failed to connect, will retry after tick {} (attempt {}/{})",
                        agent_name_for_backoff, backoff.retry_after_tick,
                        backoff.failures, SpawnBackoff::MAX_RETRIES
                    );
                }).await;
                return;
            }

            // Spawn succeeded — clear any backoff state.
            {
                let agent_name_for_log = agent_name.clone();
                ctx.run_on_main_thread(move |main_ctx| {
                    let world = main_ctx.world;
                    let mut sessions = world.resource_mut::<AgentSessions>();
                    if let Some(backoff) = sessions.spawn_backoff.get(&agent_name_for_log) {
                        if backoff.failures > 0 {
                            tracing::info!(
                                "Session for {} connected after {} prior failures",
                                agent_name_for_log, backoff.failures
                            );
                        }
                    }
                    sessions.spawn_backoff.remove(&agent_name_for_log);
                }).await;
            }

            // Send the intro message.
            let intro = build_intro_message(&agent_name);
            let _ = bridge.prompt_tx.send(intro).await;

            let ptx_clone = bridge.prompt_tx.clone();
            let rrx_clone = Arc::new(Mutex::new(bridge.response_rx));
            let token_rx_clone = Arc::new(Mutex::new(bridge.token_rx));
            let agent_uuid_for_main = agent_uuid.clone();

            ctx.run_on_main_thread(move |main_ctx| {
                let world = main_ctx.world;
                let mut sessions = world.resource_mut::<AgentSessions>();
                if let Some(s) = sessions.sessions.get_mut(&entity_copy) {
                    s.prompt_tx = ptx_clone;
                    s.response_rx = rrx_clone;
                    tracing::info!("Session channels updated for entity {:?}", entity_copy);
                }
                let mut token_queue = world.resource_mut::<TokenEventQueue>();
                token_queue.receivers.insert(agent_uuid_for_main, token_rx_clone);
            }).await;
        });
    }
}

/// System: drain agent thoughts from relay and log them.
/// These are Claude's reasoning text BEFORE tool calls — visible in the activity log.
/// Incapacitated agents still have their channel drained (to prevent backlog)
/// but thoughts are discarded — they're passed out.
pub fn ai_thought_drain_system(
    tick: Res<TickCount>,
    mut event_log: ResMut<AgentEventLog>,
    mut sessions: ResMut<AgentSessions>,
    mut agents: Query<(
        Entity,
        &AgentName,
        &mut ThoughtBubble,
        Option<&crate::world::hospital::Incapacitated>,
    )>,
) {
    for (entity, name, mut thought, incap) in &mut agents {
        let Some(session) = sessions.sessions.get_mut(&entity) else {
            continue;
        };

        // Drain all available messages from the relay.
        loop {
            let msg = {
                let mut rx = session.response_rx.lock().unwrap();
                match rx.try_recv() {
                    Ok(text) => Some(text),
                    Err(_) => None,
                }
            };
            let Some(text) = msg else { break };

            // Discard thoughts from passed-out agents — they can't think.
            if incap.is_some() {
                continue;
            }

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
/// This system provides slim vitals + nearby + tools. Agents query details via check_* actions.
pub fn ai_context_system(
    tick: Res<TickCount>,
    boards_ai: Query<&BountyTokenStore, With<BountyBoard>>,
    mut sessions: ResMut<AgentSessions>,
    agents: Query<(
        Entity,
        &AgentName,
        &GridPos,
        &AgentGoal,
        &Needs,
        &Inventory,
        &KnownLocations,
        Option<&ActionTimer>,
        Option<&Path>,
        Option<&InsideBuilding>,
        Option<&ShiftWorker>,
    ), Without<crate::world::hospital::Incapacitated>>,
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
        goal,
        needs,
        inv,
        known_locs,
        action_timer,
        path,
        inside_building,
        shift_worker,
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
                    return matches!(
                        b.state,
                        crate::world::bounty::BountyState::Claimed
                            | crate::world::bounty::BountyState::PendingVerification
                    );
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

        let inside_building_name = inside_building
            .and_then(|inside| structures.get(inside.0).ok().map(|(_, _, sprite)| sprite.0.as_str()));
        let location_tools = build_location_tools(
            inside_building_name,
            matches!(goal, AgentGoal::InteractingWithBoard),
            shift_worker.is_some(),
            matches!(goal, AgentGoal::ExecutingBounty(_)),
        );

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
            tick.0,
            &available_bounties,
            &nearby,
            &location_tools,
            active_bounty_desc.as_deref(),
        );

        // Send context as a user message. The LLM will think and call game_action tool.
        if let Err(e) = session.prompt_tx.try_send(context) {
            tracing::warn!("[AI:{}] context send failed (channel full or closed): {}", name.0, e);
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

4. YOU INTERACT THROUGH ACTIONS. Use your game_action tool. That is your ONLY way to affect the world. Use go_to with coordinates to move. Once you are standing on a building entrance, call the local action directly: buy_muffin, redeem_paycheck, read_library, sleep_hotel, and so on. Use check_own_stats, check_inventory, check_known_locations, and check_relationships to inspect your own state at any time.

5. SOCIAL CONNECTIONS MATTER. You can start conversations with nearby agents, exchange business cards, send messages, and even trade items. Chatting relieves boredom for free. But don't waste time on small talk when there's gold to earn.

6. THE SYSTEM IS WATCHING. Every bounty you submit is verified by the System — an all-seeing AI that will approve or reject your work. The System is... opinionated. Don't take it personally. Use the help action any time you need the cheat sheet, and include text if you want to file feedback.

You have been placed near the bounty board. That is not a coincidence. Your first move matters.

Good luck, {agent_name}. The clock is ticking.
"#
    )
}

/// Wait for a session to signal readiness via a watch channel.
/// Returns true if connected, false on timeout.
pub async fn wait_for_connected(
    rx: &mut tokio::sync::watch::Receiver<bool>,
    label: &str,
    timeout_secs: u64,
) -> bool {
    // Already ready (e.g. OpenAI signals immediately at construction).
    if *rx.borrow() {
        tracing::info!("Session connected for {} (immediate)", label);
        return true;
    }
    // Wait for the value to change to true.
    tokio::select! {
        result = rx.changed() => {
            if result.is_ok() && *rx.borrow() {
                tracing::info!("Session connected for {}", label);
                true
            } else {
                tracing::error!("Session connect signal lost for {}", label);
                false
            }
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            tracing::error!("Session connection timeout for {} ({}s)", label, timeout_secs);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_location_tools;

    #[test]
    fn board_tools_show_direct_redeem_action() {
        let tools = build_location_tools(Some("bounty_board"), true, false, true);

        assert!(tools.contains(&"claim_bounty"));
        assert!(tools.contains(&"redeem_paycheck"));
        assert!(tools.contains(&"complete_bounty"));
    }

    #[test]
    fn on_shift_tools_hide_duplicate_work_shift_and_fake_shift_actions() {
        let tools = build_location_tools(Some("market"), false, true, false);

        assert!(tools.contains(&"leave_shift"));
        assert!(!tools.contains(&"work_shift"));
        assert!(!tools.iter().any(|tool| matches!(
            *tool,
            "brew_coffee"
                | "sell_to_customer"
                | "stock_shelves"
                | "buy_wholesale"
                | "check_in_guest"
        )));
    }

    #[test]
    fn cafe_tools_show_real_services_not_fake_eat_alias() {
        let tools = build_location_tools(Some("cafe"), false, false, false);

        assert!(tools.contains(&"buy_muffin"));
        assert!(tools.contains(&"buy_coffee"));
        assert!(tools.contains(&"hang_out"));
        assert!(!tools.contains(&"eat_cafe"));
    }
}
