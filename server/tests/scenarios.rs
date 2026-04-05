//! Scenario integration tests.
//!
//! These tests spawn the real Bevy game server with injectable configs.
//! They are marked #[ignore] because they require a real Claude API key.
//!
//! Run with: cargo test --test scenarios -- --ignored

mod common;

use common::ScenarioHarness;
use server::world::bounty::BountyObjective;
use std::time::Duration;

#[test]
#[ignore] // requires real Claude API — run with: cargo test --test scenarios -- --ignored
fn agent_claims_and_completes_bounty() {
    let harness = ScenarioHarness::builder()
        .agent("Tester", "haiku", 3.0)
        .bounty(
            "Warehouse inventory audit",
            6,
            BountyObjective::WorkAtBuilding,
            "Instructions for agent: Go to the warehouse and use look_around to inspect the inventory. Then use create_document (service='warehouse_audit.md', text='<your findings>') to write up what you found. Return to the board.\n\nGM: STRICT — verify agent has a 'document' item in inventory AND visited the warehouse. If no document, REJECT.",
        )
        .tick_ms(50)
        .max_ticks(5000)
        .build();

    // Wait for the agent to appear in the snapshot.
    let snap = harness
        .wait_for(Duration::from_secs(10), |s| {
            s.agents.iter().any(|a| a.name == "Tester")
        })
        .expect("Agent 'Tester' never appeared in snapshot");

    assert_eq!(snap.agents.len(), 1);
    assert_eq!(snap.agents[0].name, "Tester");

    // Wait for the bounty to be seeded.
    let snap = harness
        .wait_for(Duration::from_secs(10), |s| !s.bounties.is_empty())
        .expect("Bounties never appeared");

    assert_eq!(snap.bounties.len(), 1);
    assert_eq!(snap.bounties[0].description, "Warehouse inventory audit");

    // Wait for the agent to earn gold (bounty completion).
    // This is the real test — the agent must autonomously:
    // 1. Claim the bounty at the board
    // 2. Go to the warehouse
    // 3. Look around + create a document
    // 4. Return to the board
    // 5. Complete the bounty for gold
    let result = harness.wait_for(Duration::from_secs(120), |s| {
        s.agents.iter().any(|a| a.gold >= 6)
    });

    match result {
        Ok(snap) => {
            let agent = snap.agents.iter().find(|a| a.name == "Tester").unwrap();
            println!(
                "SUCCESS: Agent earned {} gold at tick {} (position: {:?})",
                agent.gold, snap.tick, agent.position
            );
        }
        Err(msg) => {
            // Print the final state for debugging.
            let snap = harness.current_snapshot();
            println!("FINAL STATE at tick {}:", snap.tick);
            for agent in &snap.agents {
                println!(
                    "  Agent {}: gold={}, debt={}, pos={:?}, goal={}, energy={:.0}, hunger={:.0}",
                    agent.name, agent.gold, agent.gold_debt, agent.position, agent.goal,
                    agent.energy, agent.hunger,
                );
            }
            for bounty in &snap.bounties {
                println!(
                    "  Bounty '{}': {}g, state={}, claimed_by={:?}",
                    bounty.description, bounty.reward, bounty.state, bounty.claimed_by,
                );
            }
            panic!("Agent never earned gold: {}", msg);
        }
    }

    harness.shutdown();
}

/// Smoke test: verify the harness can start and the server becomes healthy.
/// This does NOT require a Claude API key — it just checks the game loop runs.
#[test]
fn harness_starts_and_shuts_down() {
    let harness = ScenarioHarness::builder()
        .agent("Smoketest", "haiku", 2.0)
        .tick_ms(50)
        .max_ticks(100)
        .build();

    // Wait for at least 1 tick to elapse.
    let snap = harness
        .wait_for(Duration::from_secs(10), |s| s.tick > 0)
        .expect("Game loop never ticked");

    assert!(snap.tick > 0, "Expected at least 1 tick, got {}", snap.tick);

    // Verify the agent was spawned.
    let snap = harness
        .wait_for(Duration::from_secs(10), |s| !s.agents.is_empty())
        .expect("Agent never appeared");

    assert_eq!(snap.agents.len(), 1);
    assert_eq!(snap.agents[0].name, "Smoketest");

    // Let it auto-shutdown via MaxTicks.
    let snap = harness
        .wait_for(Duration::from_secs(30), |s| s.tick >= 90)
        .expect("Game loop never reached near max ticks");

    println!("Harness ran to tick {}", snap.tick);
}

/// Verify that scenario bounties are seeded (not the defaults).
#[test]
fn scenario_bounties_override_defaults() {
    let harness = ScenarioHarness::builder()
        .agent("Auditor", "haiku", 2.0)
        .bounty(
            "Custom test bounty",
            99,
            BountyObjective::WorkAtBuilding,
            "test criteria",
        )
        .tick_ms(50)
        .max_ticks(200)
        .build();

    let snap = harness
        .wait_for(Duration::from_secs(10), |s| !s.bounties.is_empty())
        .expect("Bounties never appeared");

    // Should have exactly 1 bounty (our custom one), not the 9 defaults.
    assert_eq!(snap.bounties.len(), 1, "Expected 1 scenario bounty, got {}", snap.bounties.len());
    assert_eq!(snap.bounties[0].description, "Custom test bounty");
    assert_eq!(snap.bounties[0].reward, 99);

    harness.shutdown();
}
