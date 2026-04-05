use bevy::prelude::*;
use bevy_tokio_tasks::TokioTasksRuntime;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

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
    pub system_prompt: String,
}

/// Bevy resource wrapping AgentRelays for Axum.
#[derive(Resource, Clone)]
pub struct AgentRelaysResource(pub AgentRelays);

/// How often to send context updates (in ticks). 50 ticks = 5 seconds.
const CONTEXT_INTERVAL: u64 = 50;

/// System: spawn Claude sessions via --sdk-url for agents.
pub fn spawn_sessions_system(
    runtime: ResMut<TokioTasksRuntime>,
    mut sessions: ResMut<AgentSessions>,
    relays: Res<AgentRelaysResource>,
    agents: Query<(Entity, &AgentId, &AgentName, &Personality, &ClaudeModel)>,
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

        let (placeholder_tx, _) = mpsc::channel(1);
        let (_, placeholder_rx) = mpsc::channel(1);
        sessions.sessions.insert(entity, SessionState {
            prompt_tx: placeholder_tx,
            response_rx: Arc::new(Mutex::new(placeholder_rx)),
            last_decision_tick: 0,
            system_prompt: system_prompt.clone(),
        });

        let entity_copy = entity;

        runtime.spawn_background_task(move |mut ctx| async move {
            let handle = relays_clone.register(&agent_uuid).await;

            let prompt_file = format!("/tmp/agent-{}.md", agent_uuid);
            if let Err(e) = tokio::fs::write(&prompt_file, &sys_prompt_clone).await {
                tracing::error!("Failed to write prompt file: {}", e);
                return;
            }

            // Per-agent MCP config with identity baked in.
            // Use absolute path based on the binary's own location, not cwd.
            let mcp_binary = std::env::current_exe()
                .map(|exe| exe.parent().unwrap().join("mcp-game").to_string_lossy().to_string())
                .unwrap_or_else(|_| "/Users/art/code/stripe-prototype/target/debug/mcp-game".into());
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

            let sdk_url = format!("ws://127.0.0.1:8080/agent/{}/ws", agent_uuid);
            let mut env: HashMap<String, String> = std::env::vars().collect();
            env.remove("ANTHROPIC_API_KEY");

            let child = tokio::process::Command::new("claude")
                .args([
                    "--sdk-url", &sdk_url,
                    "--output-format", "stream-json",
                    "--input-format", "stream-json",
                    "--permission-mode", "bypassPermissions",
                    "--model", &model_name,
                    "--append-system-prompt-file", &prompt_file,
                    "--mcp-config", &mcp_config_path,
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

            tracing::info!("Claude spawned for {} → {} (model: {})", agent_name, sdk_url, model_name);

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

            let _ = child.wait().await;
            tracing::info!("Claude process exited for {}", agent_name);
            let _ = tokio::fs::remove_file(&prompt_file).await;
            let _ = tokio::fs::remove_file(&mcp_config_path).await;
        });
    }
}

/// System: send context updates to Claude sessions.
/// Claude acts via the MCP game_action tool — we do NOT parse responses.
/// This system only provides situational awareness.
pub fn ai_context_system(
    tick: Res<TickCount>,
    bounty_registry: Res<BountyRegistry>,
    mut sessions: ResMut<AgentSessions>,
    agents: Query<(
        Entity, &AgentName, &GridPos, &Speed,
        &AgentGoal, &Needs, &Inventory, &KnownLocations, &Relationships,
        Option<&ActionTimer>, Option<&Path>,
        Option<&InsideBuilding>, Option<&ShiftWorker>,
    )>,
    all_agents: Query<(&AgentName, &GridPos)>,
    structures: Query<(Entity, &Entrance, &SpriteType), With<StructureId>>,
) {
    for (
        entity, name, pos, speed, goal, needs, inv, known_locs, rels,
        action_timer, path, inside_building, shift_worker,
    ) in &agents {
        if action_timer.is_some() { continue; }
        let has_path = path.is_some_and(|p| !p.0.is_empty());
        if has_path { continue; }

        let Some(session) = sessions.sessions.get_mut(&entity) else { continue };

        // Rate limit context updates.
        if tick.0 - session.last_decision_tick < CONTEXT_INTERVAL { continue; }

        // Only send context when agent can act.
        if !matches!(goal,
            AgentGoal::Idle | AgentGoal::Wandering |
            AgentGoal::WorkingShift { .. } | AgentGoal::ExecutingBounty(_)
        ) { continue; }

        // Only show bounties if at the board.
        let at_board = inside_building.map_or(false, |ib| {
            structures.get(ib.0).map_or(false, |(_, _, s)| s.0 == "bounty_board")
        });
        let available_bounties: Vec<String> = if at_board {
            bounty_registry.available().iter()
                .map(|b| format!("{} ({}g) — {}", b.description, b.reward_gold,
                    match &b.objective {
                        crate::world::bounty::BountyObjective::HideItem(item) =>
                            format!("You'll receive a {} to hide in any structure.", item),
                        crate::world::bounty::BountyObjective::FindItem(item) =>
                            format!("Search structures to find a hidden {}.", item),
                        crate::world::bounty::BountyObjective::RestockDelivery { item, quantity, destination } =>
                            format!("Buy {} {} from warehouse and deliver to {}.", quantity, item, destination),
                        crate::world::bounty::BountyObjective::WorkAtBuilding =>
                            "Go to the specified building and complete the task.".into(),
                    }
                )).collect()
        } else {
            vec![]
        };

        let nearby: Vec<(String, GridPos)> = all_agents.iter()
            .filter(|(n, _)| n.0 != name.0)
            .filter(|(_, p)| (p.x - pos.x).abs() + (p.y - pos.y).abs() <= 10)
            .map(|(n, p)| (n.0.clone(), *p)).collect();

        // Location-specific tools.
        let mut location_tools: Vec<&str> = vec!["look_around", "wander", "go_to_board", "go_to_service", "work_shift"];
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
        if matches!(goal, AgentGoal::ExecutingBounty(_)) {
            location_tools.push("complete_bounty");
        }

        // Get active bounty description.
        let active_bounty_desc = match goal {
            AgentGoal::ExecutingBounty(bid) => {
                bounty_registry.get(*bid).map(|b| b.description.clone())
            }
            _ => None,
        };

        let context = super::ai_decision::build_context(
            &name.0, pos, needs, inv, goal, known_locs, rels,
            speed.0, &available_bounties, &nearby, &location_tools,
            active_bounty_desc.as_deref(),
        );

        // Send context as a user message. Claude will think and call game_action tool.
        if let Err(e) = session.prompt_tx.try_send(context) {
            tracing::debug!("[AI:{}] context send failed: {}", name.0, e);
            continue;
        }

        session.last_decision_tick = tick.0;
    }
}
