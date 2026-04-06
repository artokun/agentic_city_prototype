use bevy::prelude::*;
use std::collections::HashMap;
use uuid::Uuid;

use server::agents::ai::AgentSessions;
use server::agents::components::AgentGoal;
use server::agents::components::{AgentId, AgentName, ThoughtBubble};
use server::agents::event_log::AgentEventLog;
use server::agents::needs::Needs;
use server::items::{
    ContainedItems, DocumentInventory, Inventory, ItemContents, ItemKind, ItemName, ItemType,
};
use server::network::action_handler::{
    deliver_documents_system, process_create_documents_system, PendingCreateDocument,
};
use server::network::commands::PendingDocuments;
use server::tick::TickCount;
use server::world::bounty::{BoardQueue, BountyBoard, BountyTokenStore};
use server::world::bounty_injector::bounty_injection_system;
use server::world::map::{GridPos, TileType, WorldMap};

/// Create a minimal Bevy App for testing with a 10x10 all-walkable map.
fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);

    // Build a 10x10 all-walkable map.
    let mut tiles = HashMap::new();
    for x in 0..10 {
        for y in 0..10 {
            tiles.insert(GridPos { x, y }, TileType::Street);
        }
    }
    app.insert_resource(WorldMap { tiles });
    app.insert_resource(TickCount(0));
    // Spawn a bounty board entity with BountyTokenStore component.
    app.world_mut()
        .spawn((BountyBoard, BountyTokenStore::default()));
    app.init_resource::<AgentEventLog>();

    app
}

#[test]
fn bounty_injection_seeds_initial_bounties() {
    let mut app = test_app();
    app.init_resource::<server::world::bounty_injector::InjectorState>();
    app.add_systems(Update, bounty_injection_system);

    // Tick 0 seeds bounties.
    app.update();

    // Find the board entity's BountyTokenStore component.
    let available = {
        let mut q = app.world_mut().query::<&BountyTokenStore>();
        let store = q
            .iter(app.world())
            .next()
            .expect("Board entity should exist");
        let avail = store.available().len();
        assert!(
            avail >= 1,
            "Expected at least 1 initial bounty, got {avail}"
        );
        store.tokens.len()
    };

    // Running again should NOT add more (one-shot).
    let count_before = available;
    {
        let mut tick = app.world_mut().resource_mut::<TickCount>();
        tick.0 += 1;
    }
    app.update();
    let count_after = {
        let mut q = app.world_mut().query::<&BountyTokenStore>();
        let store = q
            .iter(app.world())
            .next()
            .expect("Board entity should exist");
        store.tokens.len()
    };
    assert_eq!(
        count_after, count_before,
        "Injector should not create more bounties after seeding"
    );
}

#[test]
fn needs_decay_over_time() {
    use server::agents::needs::needs_decay_system;

    let mut app = test_app();
    app.add_systems(Update, needs_decay_system);

    // Spawn an agent with default Needs and Idle goal.
    let agent = app
        .world_mut()
        .spawn((Needs::default(), AgentGoal::Idle))
        .id();

    let defaults = Needs::default();

    // Run 100 ticks.
    for _ in 0..100 {
        app.update();
    }

    let needs = app.world().entity(agent).get::<Needs>().unwrap();
    // Energy is now token-driven (not time-based), so it should NOT decay.
    assert_eq!(
        needs.energy, defaults.energy,
        "Energy should NOT decay over time (token-driven now): {} != {}",
        needs.energy, defaults.energy
    );
    assert!(
        needs.hunger < defaults.hunger,
        "Hunger should have decayed: {} >= {}",
        needs.hunger,
        defaults.hunger
    );
    assert!(
        needs.boredom < defaults.boredom,
        "Boredom should have decayed (agent is Idle): {} >= {}",
        needs.boredom,
        defaults.boredom
    );
}

#[test]
fn board_queue_serializes_access() {
    let mut app = test_app();

    // Spawn a board entity and two dummy agent entities.
    let board = app.world_mut().spawn(BoardQueue::default()).id();
    let agent_a = app.world_mut().spawn_empty().id();
    let agent_b = app.world_mut().spawn_empty().id();

    // Agent A gets exclusive access.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        assert!(queue.try_interact(agent_a), "Agent A should get access");
    }

    // Agent B is blocked while A is interacting.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        assert!(
            !queue.try_interact(agent_b),
            "Agent B should be blocked while A is interacting"
        );
    }

    // Agent A can re-acquire (idempotent).
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        assert!(
            queue.try_interact(agent_a),
            "Agent A should keep access on re-acquire"
        );
    }

    // Agent B joins the wait queue.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        queue.join_queue(agent_b);
        assert_eq!(queue.waiting.len(), 1, "Agent B should be in the queue");
    }

    // Joining again is idempotent.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        queue.join_queue(agent_b);
        assert_eq!(
            queue.waiting.len(),
            1,
            "Duplicate join should not add again"
        );
    }

    // No advance while someone is interacting.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        assert!(
            queue.advance().is_none(),
            "Advance should return None while A is interacting"
        );
    }

    // Agent A leaves.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        queue.leave(agent_a);
        assert!(
            queue.interacting.is_none(),
            "Board should have no interactor after A leaves"
        );
    }

    // Advance promotes B.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        let next = queue.advance();
        assert_eq!(
            next,
            Some(agent_b),
            "Advance should promote Agent B from the queue"
        );
        assert_eq!(
            queue.interacting,
            Some(agent_b),
            "Agent B should now be the interactor"
        );
        assert!(
            queue.waiting.is_empty(),
            "Wait queue should be empty after advance"
        );
    }

    // Agent B leaves, board is fully clear.
    {
        let mut queue = app.world_mut().get_mut::<BoardQueue>(board).unwrap();
        queue.leave(agent_b);
        assert!(queue.interacting.is_none(), "Board should be clear");
        assert!(queue.waiting.is_empty(), "Queue should be empty");
    }
}

#[test]
fn create_document_upserts_existing_item_entity() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_systems(Update, process_create_documents_system);

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Writer".into()),
            AgentId(Uuid::new_v4()),
            Inventory::default(),
            DocumentInventory::default(),
            ContainedItems::default(),
            PendingCreateDocument {
                title: "note.md".into(),
                content: "first pass".into(),
            },
        ))
        .id();

    app.update();

    let item_entity = {
        let world = app.world();
        let inv = world.entity(agent).get::<Inventory>().unwrap();
        let docs = world.entity(agent).get::<DocumentInventory>().unwrap();
        let contained = world.entity(agent).get::<ContainedItems>().unwrap();
        assert_eq!(inv.count(ItemType::Document), 1);
        assert_eq!(
            docs.documents.get("note.md").map(String::as_str),
            Some("first pass")
        );
        assert_eq!(contained.items.len(), 1);
        contained.items[0]
    };

    {
        let item = app.world().entity(item_entity);
        assert_eq!(item.get::<ItemKind>().unwrap().0, ItemType::Document);
        assert_eq!(item.get::<ItemName>().unwrap().0, "note.md");
        assert_eq!(item.get::<ItemContents>().unwrap().0, "first pass");
    }

    app.world_mut()
        .entity_mut(agent)
        .insert(PendingCreateDocument {
            title: "note.md".into(),
            content: "second pass".into(),
        });
    app.update();

    let world = app.world();
    let inv = world.entity(agent).get::<Inventory>().unwrap();
    let docs = world.entity(agent).get::<DocumentInventory>().unwrap();
    let contained = world.entity(agent).get::<ContainedItems>().unwrap();
    assert_eq!(
        inv.count(ItemType::Document),
        1,
        "upsert should not duplicate document count"
    );
    assert_eq!(
        docs.documents.get("note.md").map(String::as_str),
        Some("second pass")
    );
    assert_eq!(
        contained.items.len(),
        1,
        "upsert should reuse the same item entity"
    );
    let item = world.entity(contained.items[0]);
    assert_eq!(item.get::<ItemContents>().unwrap().0, "second pass");
}

#[test]
fn delivered_documents_flow_into_item_creation_pipeline() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<PendingDocuments>();
    app.init_resource::<AgentSessions>();
    app.init_resource::<AgentEventLog>();
    app.insert_resource(TickCount(7));
    app.add_systems(
        Update,
        (
            deliver_documents_system,
            process_create_documents_system.after(deliver_documents_system),
        ),
    );

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Researcher".into()),
            AgentId(Uuid::new_v4()),
            ThoughtBubble::default(),
            Inventory::default(),
            DocumentInventory::default(),
            ContainedItems::default(),
        ))
        .id();

    app.world_mut()
        .resource_mut::<PendingDocuments>()
        .docs
        .push((
            "Researcher".into(),
            "research.md".into(),
            "egyptian cats".into(),
        ));

    app.update();

    let world = app.world();
    let thought = world.entity(agent).get::<ThoughtBubble>().unwrap();
    let inv = world.entity(agent).get::<Inventory>().unwrap();
    let docs = world.entity(agent).get::<DocumentInventory>().unwrap();
    let contained = world.entity(agent).get::<ContainedItems>().unwrap();

    assert!(thought.0.contains("research.md"));
    assert_eq!(inv.count(ItemType::Document), 1);
    assert_eq!(
        docs.documents.get("research.md").map(String::as_str),
        Some("egyptian cats")
    );
    assert_eq!(contained.items.len(), 1);
    let item = world.entity(contained.items[0]);
    assert_eq!(item.get::<ItemName>().unwrap().0, "research.md");
    assert_eq!(item.get::<ItemContents>().unwrap().0, "egyptian cats");
}
