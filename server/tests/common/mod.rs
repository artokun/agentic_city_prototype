//! Scenario test harness for the Bevy ECS game server.
//!
//! Spawns a full Bevy App in a background thread with:
//! - OS-assigned port for isolation (parallel test safety)
//! - Custom agent and bounty configs
//! - Shared `Arc<Mutex<WorldSnapshot>>` for assertions
//! - Auto-shutdown after max ticks or via Drop

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use server::items::ItemType;
use server::scenario::*;
use server::world::bounty::BountyObjective;

/// Builder for constructing a scenario test harness.
pub struct ScenarioBuilder {
    agents: Vec<ScenarioAgent>,
    bounties: Vec<ScenarioBounty>,
    tick_ms: u64,
    max_ticks: u64,
}

impl ScenarioBuilder {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            bounties: Vec::new(),
            tick_ms: 50,
            max_ticks: 5000,
        }
    }

    pub fn agent(mut self, name: &str, model: &str, speed: f32) -> Self {
        self.agents.push(ScenarioAgent {
            name: name.to_string(),
            model: model.to_string(),
            speed,
        });
        self
    }

    pub fn bounty(
        mut self,
        title: &str,
        reward: u32,
        objective: BountyObjective,
        hidden_criteria: &str,
    ) -> Self {
        self.bounties.push(ScenarioBounty {
            title: title.to_string(),
            reward,
            hidden_criteria: hidden_criteria.to_string(),
            objective,
            claim_items: Vec::new(),
        });
        self
    }

    #[allow(dead_code)]
    pub fn bounty_with_items(
        mut self,
        title: &str,
        reward: u32,
        objective: BountyObjective,
        hidden_criteria: &str,
        claim_items: Vec<(ItemType, u32)>,
    ) -> Self {
        self.bounties.push(ScenarioBounty {
            title: title.to_string(),
            reward,
            hidden_criteria: hidden_criteria.to_string(),
            objective,
            claim_items,
        });
        self
    }

    pub fn tick_ms(mut self, ms: u64) -> Self {
        self.tick_ms = ms;
        self
    }

    pub fn max_ticks(mut self, ticks: u64) -> Self {
        self.max_ticks = ticks;
        self
    }

    pub fn build(self) -> ScenarioHarness {
        // Pick a random available port via OS assignment.
        let listener = TcpListener::bind("0.0.0.0:0").expect("Failed to bind to ephemeral port");
        let port = listener.local_addr().unwrap().port();
        // Drop the listener so the port is free for the Bevy app's axum server.
        drop(listener);

        let snapshot = Arc::new(Mutex::new(WorldSnapshot::default()));
        let shutdown_flag = Arc::new(Mutex::new(false));

        let snapshot_clone = snapshot.clone();
        let shutdown_clone = shutdown_flag.clone();
        let agents = self.agents;
        let bounties = self.bounties;
        let tick_ms = self.tick_ms;
        let max_ticks = self.max_ticks;

        // Create a temp directory for documents (per-test isolation).
        let test_id = uuid::Uuid::new_v4();
        let documents_dir = format!("/tmp/scenario-{}", test_id);
        let _ = std::fs::create_dir_all(&documents_dir);
        let documents_dir_clone = documents_dir.clone();

        let handle = std::thread::Builder::new()
            .name(format!("scenario-{}", port))
            .spawn(move || {
                // Initialize tracing so we can see logs during tests.
                let _ = tracing_subscriber::fmt::try_init();

                // Set DOCUMENTS_DIR for this test instance.
                std::env::set_var("DOCUMENTS_DIR", &documents_dir_clone);

                use bevy::app::ScheduleRunnerPlugin;
                use bevy::prelude::*;
                use bevy_tokio_tasks::TokioTasksPlugin;

                let mut app = App::new();

                app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(
                    Duration::from_millis(tick_ms),
                )));
                app.add_plugins(TokioTasksPlugin::default());
                app.add_plugins(server::tick::GameTickPlugin);
                app.add_plugins(server::world::WorldPlugin);
                app.add_plugins(server::agents::AgentPlugin);
                app.add_plugins(server::network::NetworkPlugin);

                // Insert scenario-specific resources.
                app.insert_resource(server::network::ws::ServerPort(port));
                app.insert_resource(ScenarioObserver(snapshot_clone));
                app.insert_resource(MaxTicks(max_ticks));
                app.insert_resource(ShutdownFlag(shutdown_clone));

                if !agents.is_empty() {
                    app.insert_resource(ScenarioAgentConfig { agents });
                }

                if !bounties.is_empty() {
                    app.insert_resource(ScenarioBountyConfig { bounties });
                }

                // Add scenario systems.
                app.add_systems(Update, snapshot_system);
                app.add_systems(Update, shutdown_system);
                app.add_systems(Update, shutdown_flag_system);
                app.add_systems(Update, seed_scenario_bounties_system);

                app.run();
            })
            .expect("Failed to spawn scenario thread");

        // Wait for the health check endpoint to be ready.
        let start = Instant::now();
        let timeout = Duration::from_secs(30);

        loop {
            if start.elapsed() > timeout {
                panic!(
                    "Scenario server on port {} did not become healthy within {:?}",
                    port, timeout
                );
            }

            match std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                Ok(_) => {
                    // Port is open, try the health endpoint.
                    let client = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_secs(2))
                        .build()
                        .unwrap();
                    let health_url = format!("http://127.0.0.1:{}/health", port);
                    if let Ok(resp) = client.get(&health_url).send() {
                        if resp.status().is_success() {
                            break;
                        }
                    }
                }
                Err(_) => {}
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        ScenarioHarness {
            port,
            snapshot,
            shutdown_flag,
            _thread: Some(handle),
            documents_dir,
        }
    }
}

pub struct ScenarioHarness {
    pub port: u16,
    pub snapshot: Arc<Mutex<WorldSnapshot>>,
    shutdown_flag: Arc<Mutex<bool>>,
    _thread: Option<std::thread::JoinHandle<()>>,
    documents_dir: String,
}

impl ScenarioHarness {
    /// Create a new builder.
    pub fn builder() -> ScenarioBuilder {
        ScenarioBuilder::new()
    }

    /// Get the current snapshot.
    pub fn current_snapshot(&self) -> WorldSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Poll the snapshot with a timeout until the predicate returns true.
    /// Returns the snapshot that satisfied the predicate, or an error message.
    pub fn wait_for<F>(&self, timeout: Duration, mut predicate: F) -> Result<WorldSnapshot, String>
    where
        F: FnMut(&WorldSnapshot) -> bool,
    {
        let start = Instant::now();
        let poll_interval = Duration::from_millis(100);

        loop {
            let snap = self.current_snapshot();
            if predicate(&snap) {
                return Ok(snap);
            }

            if start.elapsed() > timeout {
                return Err(format!(
                    "Timed out after {:?}. Last snapshot: tick={}, agents={:?}, bounties={:?}",
                    timeout,
                    snap.tick,
                    snap.agents
                        .iter()
                        .map(|a| format!("{}(gold={},pos={:?})", a.name, a.gold, a.position))
                        .collect::<Vec<_>>(),
                    snap.bounties
                        .iter()
                        .map(|b| format!("{}({}g,{})", b.description, b.reward, b.state))
                        .collect::<Vec<_>>(),
                ));
            }

            std::thread::sleep(poll_interval);
        }
    }

    /// Request graceful shutdown.
    pub fn shutdown(&self) {
        if let Ok(mut guard) = self.shutdown_flag.lock() {
            *guard = true;
        }
    }
}

impl Drop for ScenarioHarness {
    fn drop(&mut self) {
        // Signal shutdown.
        self.shutdown();

        // Wait for the thread to finish (with a timeout).
        if let Some(handle) = self._thread.take() {
            // Don't join indefinitely — the thread should exit via AppExit.
            let _ = handle.join();
        }

        // Clean up temp documents directory.
        let _ = std::fs::remove_dir_all(&self.documents_dir);
    }
}
