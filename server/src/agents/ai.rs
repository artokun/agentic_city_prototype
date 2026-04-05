use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::agents::ai_decision::{self, AgentAction};
use crate::agents::components::*;
use crate::agents::event_log::{AgentEventLog, LogEvent, LogKind};
use crate::agents::needs::Needs;
use crate::agents::perception::KnownLocations;
use crate::agents::social::Relationships;
use crate::items::Inventory;
use crate::tick::TickCount;
use crate::world::bounty::BountyRegistry;
use crate::world::map::{GridPos, WorldMap};
use crate::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};
use crate::world::shifts::ShiftWorker;
use crate::network::agent_relay::AgentRelays;

use super::actions::ActionTimer;
use super::pathfinding;
use super::personality::Personality;

/// Bevy resource: tracks sessions per agent.
#[derive(Resource, Default)]
pub struct AgentSessions {
    pub sessions: HashMap<Entity, SessionState>,
}

pub struct SessionState {
    pub prompt_tx: mpsc::Sender<String>,
    pub response_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    pub last_decision_tick: u64,
    pub pending: bool,
    pub system_prompt: String,
}

/// Bevy resource wrapping AgentRelays for Axum.
#[derive(Resource, Clone)]
pub struct AgentRelaysResource(pub AgentRelays);

const DECISION_INTERVAL: u64 = 50;

/// System: spawn Claude sessions via --sdk-url for agents.
pub fn spawn_sessions_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut sessions: ResMut<AgentSessions>,
    relays: Res<AgentRelaysResource>,
    agents: Query<(Entity, &AgentId, &AgentName, &Personality, &super::components::ClaudeModel)>,
) {
    for (entity, agent_id, name, personality, claude_model) in &agents {
        if sessions.sessions.contains_key(&entity) {
            continue;
        }

        let agent_name = name.0.clone();
        let agent_uuid = agent_id.0.to_string();
        let model_name = claude_model.0.clone();
        let system_prompt = super::personality::build_system_prompt(&name.0, personality);
        let relays_clone = relays.0.clone();
        let sys_prompt_clone = system_prompt.clone();

        tracing::info!("Spawning Claude --sdk-url session for {} ({})", agent_name, agent_uuid);

        // Create placeholder — will be replaced when relay is ready.
        let (placeholder_tx, _) = mpsc::channel(1);
        let (_, placeholder_rx) = mpsc::channel(1);
        sessions.sessions.insert(entity, SessionState {
            prompt_tx: placeholder_tx,
            response_rx: Arc::new(Mutex::new(placeholder_rx)),
            last_decision_tick: 0,
            pending: false,
            system_prompt: system_prompt.clone(),
        });

        let entity_copy = entity;

        runtime.spawn_background_task(move |mut ctx| async move {
            // Register the relay endpoint.
            let handle = relays_clone.register(&agent_uuid).await;

            // Write system prompt to temp file.
            let prompt_file = format!("/tmp/agent-{}.md", agent_uuid);
            if let Err(e) = tokio::fs::write(&prompt_file, &sys_prompt_clone).await {
                tracing::error!("Failed to write prompt file: {}", e);
                return;
            }

            // Spawn Claude CLI with --sdk-url.
            let sdk_url = format!("ws://127.0.0.1:8080/agent/{}/ws", agent_uuid);
            let mut env: HashMap<String, String> = std::env::vars().collect();
            env.remove("ANTHROPIC_API_KEY");

            // Write per-agent MCP config with identity baked in.
            let mcp_binary = std::env::current_dir()
                .map(|d| d.join("target/debug/mcp-game").to_string_lossy().to_string())
                .unwrap_or_else(|_| "target/debug/mcp-game".into());
            let mcp_config_path = format!("/tmp/mcp-{}.json", agent_uuid);
            let mcp_config_content = serde_json::json!({
                "mcpServers": {
                    "game-engine": {
                        "command": mcp_binary,
                        "args": [],
                        "env": {
                            "AGENT_NAME": agent_name.clone(),
                            "AGENT_ID": agent_uuid.clone(),
                        }
                    }
                }
            });
            let _ = std::fs::write(&mcp_config_path, serde_json::to_string_pretty(&mcp_config_content).unwrap());
            let mcp_config = mcp_config_path.clone();

            let child = tokio::process::Command::new("claude")
                .args([
                    "--sdk-url", &sdk_url,
                    "--output-format", "stream-json",
                    "--input-format", "stream-json",
                    "--permission-mode", "bypassPermissions",
                    "--model", &model_name,
                    "--append-system-prompt-file", &prompt_file,
                    "--mcp-config", &mcp_config,
                    "--verbose",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .envs(&env)
                .kill_on_drop(true)
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to spawn claude for {}: {}", agent_name, e);
                    return;
                }
            };

            tracing::info!("Claude spawned for {} → {}", agent_name, sdk_url);

            // Log stderr.
            let name_err = agent_name.clone();
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let reader = BufReader::new(stderr);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if !line.is_empty() && !line.contains("debugger") {
                            tracing::debug!("[claude:{}:stderr] {}", name_err, line);
                        }
                    }
                });
            }

            // Wait for Claude to connect to the WebSocket relay.
            tracing::info!("Waiting for Claude to connect for {}...", agent_name);
            let connected = handle.connected.clone();
            tokio::select! {
                _ = connected.notified() => {
                    tracing::info!("Claude connected via --sdk-url for {}", agent_name);
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    tracing::error!("Claude connection timeout for {}", agent_name);
                    return;
                }
            }

            // Update the session handle on the main thread.
            let ptx = handle.prompt_tx;
            let rrx = Arc::new(Mutex::new(handle.response_rx));
            let ptx_clone = ptx.clone();
            let rrx_clone = rrx.clone();

            ctx.run_on_main_thread(move |main_ctx| {
                let world = main_ctx.world;
                let mut sessions = world.resource_mut::<AgentSessions>();
                if let Some(s) = sessions.sessions.get_mut(&entity_copy) {
                    s.prompt_tx = ptx_clone;
                    s.response_rx = rrx_clone;
                    tracing::info!("Session channels updated for entity {:?}", entity_copy);
                }
            }).await;

            // Keep the task alive while Claude is running.
            let _ = child.wait().await;
            tracing::info!("Claude process exited for {}", agent_name);
            let _ = tokio::fs::remove_file(&prompt_file).await;
        });
    }
}

/// System: send decision prompts and apply responses.
pub fn ai_decision_system(
    mut commands: Commands,
    tick: Res<TickCount>,
    map: Res<WorldMap>,
    bounty_registry: Res<BountyRegistry>,
    mut sessions: ResMut<AgentSessions>,
    mut event_log: ResMut<AgentEventLog>,
    mut agents: Query<(
        Entity, &AgentName, &GridPos, &Speed,
        &mut AgentGoal, &mut ThoughtBubble,
        &Needs, &Inventory, &KnownLocations, &Relationships,
        Option<&ActionTimer>, Option<&Path>,
        Option<&InsideBuilding>, Option<&ShiftWorker>,
    )>,
    all_agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    let structure_list: Vec<(Entity, GridPos, String)> = structures
        .iter().map(|(e, ent, sprite)| (e, ent.0, sprite.0.clone())).collect();

    for (
        entity, name, pos, speed, mut goal, mut thought,
        needs, inv, known_locs, rels,
        action_timer, path,
        inside_building, shift_worker,
    ) in &mut agents {
        if action_timer.is_some() { continue; }
        let has_path = path.is_some_and(|p| !p.0.is_empty());
        if has_path { continue; }

        let Some(session) = sessions.sessions.get_mut(&entity) else { continue };

        // Check for pending response.
        if session.pending {
            let maybe_response = {
                let mut rx = session.response_rx.lock().unwrap();
                rx.try_recv().ok()
            };

            if let Some(response_text) = maybe_response {
                session.pending = false;
                let (action, thought_text) = ai_decision::parse_action(&response_text);
                thought.0 = thought_text.clone();
                tracing::info!("[AI:{}] {} → {:?}", name.0, thought_text, action);

                event_log.push(LogEvent {
                    tick: tick.0, agent: name.0.clone(),
                    kind: LogKind::Decision,
                    text: format!("{} → {:?}", thought_text, action),
                });

                match &action {
                    AgentAction::WorkShift { building } => {
                        if let Some((bld_entity, entrance, _)) = structure_list.iter().find(|(_, _, s)| s == building) {
                            *goal = AgentGoal::GoingToService {
                                building: *bld_entity, service: "work_shift".into(),
                            };
                            if let Some(p) = pathfinding::bfs(&map, *pos, *entrance) {
                                commands.entity(entity).insert(Path(p));
                            }
                        }
                    }
                    AgentAction::LeaveShift => { *goal = AgentGoal::Idle; }
                    _ => {
                        apply_action(&mut commands, entity, &action, &mut goal, pos, &map, &structure_list, known_locs);
                    }
                }
            }
            continue;
        }

        // Rate limit.
        if tick.0 - session.last_decision_tick < DECISION_INTERVAL { continue; }
        // Allow decisions when idle, wandering, working a shift, OR executing a bounty.
        if !matches!(*goal, AgentGoal::Idle | AgentGoal::Wandering | AgentGoal::WorkingShift { .. } | AgentGoal::ExecutingBounty(_)) { continue; }

        // Only show bounties if agent is at the bounty board.
        let at_board = inside_building.map_or(false, |ib| {
            structures.get(ib.0).map_or(false, |(_, _, s)| s.0 == "bounty_board")
        });
        let available_bounties: Vec<String> = if at_board {
            bounty_registry.available().iter()
                .map(|b| format!("{} ({}g) — {}", b.description, b.reward_gold,
                    match &b.objective {
                        crate::world::bounty::BountyObjective::HideItem(item) => format!("You'll receive a {} to hide in any structure.", item),
                        crate::world::bounty::BountyObjective::FindItem(item) => format!("Search structures to find a hidden {}.", item),
                        crate::world::bounty::BountyObjective::RestockDelivery { item, quantity, destination } =>
                            format!("Buy {} {} from warehouse and deliver to {}.", quantity, item, destination),
                        crate::world::bounty::BountyObjective::WorkAtBuilding => "Go to the specified building and complete the task.".into(),
                    }
                )).collect()
        } else {
            vec![] // Can't see bounties unless at the board
        };

        let nearby: Vec<(String, GridPos)> = all_agents.iter()
            .filter(|(n, _)| n.0 != name.0)
            .filter(|(_, p)| (p.x - pos.x).abs() + (p.y - pos.y).abs() <= 10)
            .map(|(n, p)| (n.0.clone(), *p)).collect();

        // Determine location-specific tools based on building type and shift status.
        let mut location_tools: Vec<&str> = vec!["look_around", "wander", "go_to_board", "go_to_service", "chat_with", "work_shift"];
        if let Some(inside) = inside_building {
            if let Ok((_, _, sprite)) = structures.get(inside.0) {
                let on_shift = shift_worker.is_some();
                match sprite.0.as_str() {
                    "google" => { location_tools.push("search_internet"); }
                    "cafe" if on_shift => { location_tools.push("brew_coffee"); location_tools.push("sell_to_customer"); }
                    "market" if on_shift => { location_tools.push("stock_shelves"); location_tools.push("sell_to_customer"); }
                    "warehouse" if on_shift => { location_tools.push("buy_wholesale"); }
                    "hotel" if on_shift => { location_tools.push("check_in_guest"); }
                    "apartments" => { location_tools.push("cook_meal"); location_tools.push("rest"); }
                    "bounty_board" => { location_tools.push("claim_bounty"); location_tools.push("redeem_paycheck"); }
                    _ => {}
                }
            }
        }
        if shift_worker.is_some() {
            location_tools.push("leave_shift");
        }
        // Add complete_bounty tool if executing a bounty.
        if matches!(*goal, AgentGoal::ExecutingBounty(_)) {
            location_tools.push("complete_bounty");
        }
        let location_tool_refs: Vec<&str> = location_tools.iter().copied().collect();

        // Get active bounty description if executing one.
        let active_bounty_desc = match &*goal {
            AgentGoal::ExecutingBounty(bid) => {
                bounty_registry.get(*bid).map(|b| b.description.clone())
            }
            _ => None,
        };

        let context = super::ai_decision::build_context(
            &name.0, pos, needs, inv, &goal, known_locs, rels,
            speed.0, &available_bounties, &nearby, &location_tool_refs,
            active_bounty_desc.as_deref(),
        );

        if let Err(e) = session.prompt_tx.try_send(context) {
            tracing::debug!("[AI:{}] prompt send failed: {}", name.0, e);
            continue;
        }

        session.pending = true;
        session.last_decision_tick = tick.0;
    }
}

fn apply_action(
    commands: &mut Commands, entity: Entity, action: &AgentAction,
    goal: &mut Mut<AgentGoal>, pos: &GridPos, map: &WorldMap,
    structures: &[(Entity, GridPos, String)], known_locs: &KnownLocations,
) {
    match action {
        AgentAction::GoToBoard => {
            **goal = AgentGoal::GoingToBoard;
            if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                if let Some(p) = pathfinding::bfs(map, *pos, board.entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::GoToService { building, service } => {
            if let Some((bld_entity, entrance, _)) = structures.iter().find(|(_, _, s)| s == building) {
                **goal = AgentGoal::GoingToService { building: *bld_entity, service: service.clone() };
                if let Some(p) = pathfinding::bfs(map, *pos, *entrance) {
                    commands.entity(entity).insert(Path(p));
                }
            }
        }
        AgentAction::LookAround => {
            commands.entity(entity).insert(super::perception::WantsToLook);
        }
        AgentAction::Wander => {
            let walkable = map.walkable_positions();
            let idx = (entity.to_bits() as usize + 7) % walkable.len();
            if let Some(p) = pathfinding::bfs(map, *pos, walkable[idx]) {
                if !p.is_empty() {
                    commands.entity(entity).insert(Path(p));
                    **goal = AgentGoal::Wandering;
                }
            }
        }
        AgentAction::ChatWith { .. } => { **goal = AgentGoal::Wandering; }
        AgentAction::SendMessage { recipient, text } => {
            commands.entity(entity).insert(super::mailbox::WantsToSendMessage {
                recipient_name: recipient.clone(),
                text: text.clone(),
            });
        }
        AgentAction::WorkShift { .. } | AgentAction::LeaveShift => {}
        AgentAction::CompleteBounty => {
            // Agent believes bounty is done — return to board.
            if let AgentGoal::ExecutingBounty(bounty_id) = **goal {
                **goal = AgentGoal::ReturningToBoard(bounty_id);
                if let Some(board) = known_locs.locations.values().find(|l| l.name == "bounty_board") {
                    if let Some(p) = super::pathfinding::bfs(map, *pos, board.entrance) {
                        commands.entity(entity).insert(super::components::Path(p));
                    }
                }
            }
        }
        AgentAction::DoNothing => {}
    }
}
