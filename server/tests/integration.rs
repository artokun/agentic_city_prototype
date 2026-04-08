use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

use server::agents::action_log::ActionLog;
use server::agents::ai::{AgentSessions, SessionState};
use server::agents::behavior::execution_system;
use server::agents::components::AgentGoal;
use server::agents::components::{
    AgentAnimation, AgentId, AgentName, BusinessCards, Speed, ThoughtBubble,
};
use server::agents::event_log::{AgentEventLog, LogKind};
use server::agents::gm::SystemAiState;
use server::agents::needs::Needs;
use server::agents::perception::KnownLocations;
use server::agents::token_tracking::ContextWindow;
use server::items::{
    ContainedItems, DocumentInventory, Inventory, ItemContents, ItemKind, ItemName, ItemType,
};
use server::network::action_handler::{
    apply_mcp_actions_system, deliver_documents_system, give_claim_items_system,
    process_create_documents_system, process_deposits_system, process_gm_verdicts_system,
    MpcAction, PendingActions, PendingCreateDocument, PendingDeposit, SuggestionBox,
};
use server::network::commands::{PendingDocuments, PendingVerdicts};
use server::tick::TickCount;
use server::world::bounty::{
    BoardQueue, Bounty, BountyBoard, BountyDropbox, BountyObjective, BountyState,
    BountyTokenStore, Library,
};
use server::world::bounty_injector::bounty_injection_system;
use server::world::map::{GridPos, TileType, WorldMap};
use server::world::shifts::{shift_tracking_system, PaycheckWallet, ShiftWorker, Staffable};
use server::world::structures::{Entrance, InsideBuilding, SpriteType, StructureId};

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

fn test_map() -> WorldMap {
    let mut tiles = HashMap::new();
    for x in 0..10 {
        for y in 0..10 {
            tiles.insert(GridPos { x, y }, TileType::Street);
        }
    }
    WorldMap { tiles }
}

fn spawn_bounty_board(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            BountyBoard,
            BoardQueue::default(),
            BountyTokenStore::default(),
            BountyDropbox::default(),
            StructureId(Uuid::new_v4()),
            SpriteType("bounty_board".into()),
            Entrance(GridPos { x: 0, y: 0 }),
            Inventory::default(),
            DocumentInventory::default(),
            ContainedItems::default(),
        ))
        .id()
}

fn spawn_structure(
    app: &mut App,
    name: &str,
    entrance: GridPos,
    staffable: Option<Staffable>,
) -> Entity {
    let mut entity = app.world_mut().spawn((
        StructureId(Uuid::new_v4()),
        SpriteType(name.into()),
        Entrance(entrance),
        Inventory::default(),
        DocumentInventory::default(),
        ContainedItems::default(),
    ));
    if let Some(staffable) = staffable {
        entity.insert(staffable);
    }
    entity.id()
}

fn action_test_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(test_map());
    app.insert_resource(TickCount(0));
    app.init_resource::<AgentEventLog>();
    app.init_resource::<PendingActions>();
    app.init_resource::<SuggestionBox>();
    app.init_resource::<AgentSessions>();
    app.insert_resource(Library::default());
    spawn_bounty_board(&mut app);
    app
}

fn gm_test_app() -> App {
    let mut app = action_test_app();
    app.init_resource::<PendingVerdicts>();
    app.init_resource::<SystemAiState>();
    app.insert_resource(Library::default());
    app.add_systems(Update, process_gm_verdicts_system);
    app.add_systems(Update, give_claim_items_system);
    app.add_systems(Update, process_create_documents_system);
    app
}

fn attach_test_session(app: &mut App, entity: Entity) -> mpsc::Receiver<String> {
    let (prompt_tx, prompt_rx) = mpsc::channel(8);
    let (_response_tx, response_rx) = mpsc::channel(1);
    app.world_mut()
        .resource_mut::<AgentSessions>()
        .sessions
        .insert(
            entity,
            SessionState {
                prompt_tx,
                response_rx: Arc::new(Mutex::new(response_rx)),
                last_decision_tick: 0,
                system_prompt: String::new(),
            },
        );
    prompt_rx
}

fn spawn_gm_test_agent(app: &mut App, name: &str) -> Entity {
    app.world_mut()
        .spawn((
            AgentName(name.into()),
            AgentId(Uuid::new_v4()),
            GridPos { x: 0, y: 0 },
            ThoughtBubble::default(),
            Inventory::default(),
            Needs::default(),
            KnownLocations::default(),
            AgentGoal::Idle,
            BusinessCards::default(),
            ContextWindow::default(),
            DocumentInventory::default(),
            ContainedItems::default(),
        ))
        .id()
}

#[test]
fn bounty_injection_seeds_initial_bounties() {
    // Write embedded test bounties to a temp file so the test doesn't depend
    // on bounties.json existing at the project root.
    let test_bounties = serde_json::json!([{
        "title": "Test Bounty",
        "instructions": "Do the thing",
        "hidden_criteria": "GM: approve if done",
        "objective": "WorkAtBuilding",
        "reward": 10
    }]);
    let tmp = std::env::temp_dir().join("test_bounties.json");
    std::fs::write(&tmp, test_bounties.to_string()).expect("write temp bounties");
    std::env::set_var("BOUNTIES_FILE", &tmp);

    let mut app = test_app();
    app.init_resource::<server::world::bounty_injector::InjectorState>();
    app.add_systems(Update, bounty_injection_system);

    // Tick 0 seeds bounties.
    app.update();

    // Clean up env var before any assertions so it doesn't leak on failure.
    std::env::remove_var("BOUNTIES_FILE");

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

#[test]
fn reissuing_same_shift_keeps_worker_active_and_accumulating() {
    let mut app = action_test_app();
    app.add_systems(Update, apply_mcp_actions_system);
    app.add_systems(Update, shift_tracking_system.after(apply_mcp_actions_system));

    let market = spawn_structure(
        &mut app,
        "market",
        GridPos { x: 3, y: 3 },
        Some(Staffable {
            ticks_per_gold: 120,
            worker: None,
            needs_worker: false,
            food_perk: true,
        }),
    );

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Alice".into()),
            GridPos { x: 3, y: 3 },
            AgentGoal::WorkingShift { building: market },
            ThoughtBubble::default(),
            KnownLocations::default(),
            ShiftWorker {
                building: market,
                building_name: "market".into(),
                ticks_worked: 0,
                ticks_per_gold: 120,
            },
            Needs::default(),
            BusinessCards::default(),
            Inventory::default(),
            ContextWindow::default(),
            PaycheckWallet::default(),
        ))
        .id();

    app.world_mut().resource_mut::<PendingActions>().actions.push(MpcAction {
        agent_name: "Alice".into(),
        agent_id: String::new(),
        action: "work_shift".into(),
        building: Some("market".into()),
        service: None,
        agent_target: None,
        text: None,
        feedback: None,
        x: None,
        y: None,
    });

    app.update();

    let entity = app.world().entity(agent);
    let shift = entity.get::<ShiftWorker>().expect("shift should still be active");
    assert_eq!(shift.building, market);
    assert_eq!(shift.ticks_worked, 1);
    assert!(matches!(
        entity.get::<AgentGoal>(),
        Some(AgentGoal::WorkingShift { building }) if *building == market
    ));
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Paycheck),
        0
    );
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("Already working a shift at market")
    );
}

#[test]
fn reissuing_different_shift_is_rejected_without_losing_current_shift() {
    let mut app = action_test_app();
    app.add_systems(Update, apply_mcp_actions_system);
    app.add_systems(Update, shift_tracking_system.after(apply_mcp_actions_system));

    let market = spawn_structure(
        &mut app,
        "market",
        GridPos { x: 3, y: 3 },
        Some(Staffable {
            ticks_per_gold: 120,
            worker: None,
            needs_worker: false,
            food_perk: true,
        }),
    );
    spawn_structure(
        &mut app,
        "hotel",
        GridPos { x: 5, y: 5 },
        Some(Staffable {
            ticks_per_gold: 120,
            worker: None,
            needs_worker: false,
            food_perk: false,
        }),
    );

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Alice".into()),
            GridPos { x: 3, y: 3 },
            AgentGoal::WorkingShift { building: market },
            ThoughtBubble::default(),
            KnownLocations::default(),
            ShiftWorker {
                building: market,
                building_name: "market".into(),
                ticks_worked: 0,
                ticks_per_gold: 120,
            },
            Needs::default(),
            BusinessCards::default(),
            Inventory::default(),
            ContextWindow::default(),
            PaycheckWallet::default(),
        ))
        .id();

    app.world_mut().resource_mut::<PendingActions>().actions.push(MpcAction {
        agent_name: "Alice".into(),
        agent_id: String::new(),
        action: "work_shift".into(),
        building: Some("hotel".into()),
        service: None,
        agent_target: None,
        text: None,
        feedback: None,
        x: None,
        y: None,
    });

    app.update();

    let entity = app.world().entity(agent);
    let shift = entity.get::<ShiftWorker>().expect("shift should still be active");
    assert_eq!(shift.building, market);
    assert_eq!(shift.ticks_worked, 1);
    assert!(matches!(
        entity.get::<AgentGoal>(),
        Some(AgentGoal::WorkingShift { building }) if *building == market
    ));
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("Use leave_shift before switching to hotel")
    );
}

#[test]
fn paycheck_deposit_to_bounty_board_is_rejected_without_mutating_state() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_systems(Update, process_deposits_system);
    app.insert_resource(TickCount(0));
    app.init_resource::<AgentEventLog>();

    let board = spawn_bounty_board(&mut app);
    let agent = app
        .world_mut()
        .spawn((
            AgentName("Carol".into()),
            {
                let mut inv = Inventory::default();
                inv.add(ItemType::Paycheck, 1);
                inv
            },
            DocumentInventory::default(),
            ThoughtBubble::default(),
            ContainedItems::default(),
            PendingDeposit {
                item_name: "paycheck".into(),
                building_entity: board,
            },
        ))
        .id();

    app.update();

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Paycheck),
        1
    );
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("use redeem_paycheck instead")
    );
    assert!(entity.get::<PendingDeposit>().is_none());

    let dropbox = app
        .world()
        .entity(board)
        .get::<BountyDropbox>()
        .expect("board dropbox present");
    assert!(dropbox.get_slot(agent).is_none());
}

#[test]
fn board_rejects_regular_item_submission_without_active_bounty() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_systems(Update, process_deposits_system);
    app.insert_resource(TickCount(0));
    app.init_resource::<AgentEventLog>();

    let board = spawn_bounty_board(&mut app);
    let agent = app
        .world_mut()
        .spawn((
            AgentName("Bob".into()),
            {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 1);
                inv
            },
            DocumentInventory::default(),
            ThoughtBubble::default(),
            ContainedItems::default(),
            PendingDeposit {
                item_name: "coffee".into(),
                building_entity: board,
            },
        ))
        .id();

    app.update();

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Coffee),
        1
    );
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("Claim a bounty first")
    );

    let dropbox = app
        .world()
        .entity(board)
        .get::<BountyDropbox>()
        .expect("board dropbox present");
    assert!(dropbox.get_slot(agent).is_none());
}

#[test]
fn board_accepts_explicit_proof_item_for_active_bounty() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_systems(Update, process_deposits_system);
    app.insert_resource(TickCount(0));
    app.init_resource::<AgentEventLog>();

    let board = spawn_bounty_board(&mut app);
    let agent = app
        .world_mut()
        .spawn((
            AgentName("Dave".into()),
            {
                let mut inv = Inventory::default();
                inv.add(ItemType::Coffee, 1);
                inv
            },
            DocumentInventory::default(),
            ThoughtBubble::default(),
            ContainedItems::default(),
            PendingDeposit {
                item_name: "coffee".into(),
                building_entity: board,
            },
        ))
        .id();

    app.world_mut()
        .entity_mut(board)
        .get_mut::<BountyTokenStore>()
        .expect("board store present")
        .tokens
        .insert(
            Uuid::new_v4(),
            server::world::bounty::BountyTokenData {
                id: Uuid::new_v4(),
                description: "Submit coffee to the board".into(),
                objective: server::world::bounty::BountyObjective::FindItem(ItemType::Coffee),
                reward_gold: 5,
                state: BountyState::Claimed,
                claimed_by: Some(agent),
                claim_items: vec![],
                created_tick: 0,
                picked_up_tick: Some(0),
                ttl_ticks: 0,
                expired: false,
                steps: vec![],
                hidden_criteria: "Deposit coffee at the board as proof.".into(),
            },
        );

    app.update();

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Coffee),
        0
    );

    let dropbox = app
        .world()
        .entity(board)
        .get::<BountyDropbox>()
        .expect("board dropbox present");
    let slot = dropbox.get_slot(agent).expect("dropbox slot created");
    assert_eq!(slot.items, vec![(ItemType::Coffee, 1)]);
}

#[test]
fn complete_bounty_marks_pending_review_immediately() {
    let mut app = action_test_app();
    app.add_systems(Update, apply_mcp_actions_system);

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Alice Haiku".into()),
            AgentId(Uuid::new_v4()),
            GridPos { x: 0, y: 0 },
            ThoughtBubble::default(),
            Inventory::default(),
            Needs::default(),
            KnownLocations::default(),
            AgentGoal::ExecutingBounty(Uuid::nil()),
            BusinessCards::default(),
            ContextWindow::default(),
            DocumentInventory::default(),
            ContainedItems::default(),
        ))
        .id();

    let bounty_id = Uuid::new_v4();
    {
        let mut query = app
            .world_mut()
            .query::<(&mut BountyTokenStore, &mut BountyDropbox)>();
        let (mut store, mut dropbox) = query
            .single_mut(app.world_mut())
            .expect("board store/dropbox present");
        let mut bounty = Bounty::simple(
            bounty_id,
            "Test rescue".into(),
            BountyObjective::WorkAtBuilding,
            5,
            vec![],
        );
        bounty.claimed_by = Some(agent);
        bounty.state = BountyState::Claimed;
        bounty.picked_up_tick = Some(0);
        store.tokens.insert(bounty_id, bounty);
        dropbox.deposit_token(agent, bounty_id, None);
    }

    app.world_mut()
        .resource_mut::<PendingActions>()
        .actions
        .push(MpcAction {
            agent_name: "Alice Haiku".into(),
            agent_id: Uuid::new_v4().to_string(),
            action: "complete_bounty".into(),
            building: None,
            service: None,
            agent_target: None,
            text: None,
            feedback: None,
            x: None,
            y: None,
        });

    app.update();

    let mut query = app.world_mut().query::<(&BountyTokenStore, &BountyDropbox)>();
    let (store, dropbox) = query
        .single(app.world())
        .expect("board store/dropbox present");
    let bounty = store.tokens.get(&bounty_id).expect("bounty should exist");
    let entity = app.world().entity(agent);

    assert_eq!(bounty.state, BountyState::PendingVerification);
    assert!(dropbox.get_slot(agent).is_some());
    assert!(entity.contains::<server::agents::gm::PendingGmReview>());
    assert!(matches!(entity.get::<AgentGoal>(), Some(AgentGoal::Idle)));
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("Game Master review")
    );
}

#[test]
fn gm_approval_sends_direct_message_and_logs_chat_and_activity() {
    let mut app = gm_test_app();
    let agent = spawn_gm_test_agent(&mut app, "Alice Haiku");
    let mut prompt_rx = attach_test_session(&mut app, agent);
    let bounty_id = Uuid::new_v4();

    {
        let mut query = app
            .world_mut()
            .query::<(&mut BountyTokenStore, &mut BountyDropbox)>();
        let (mut store, mut dropbox) = query
            .single_mut(app.world_mut())
            .expect("board store/dropbox present");
        let mut bounty = Bounty::simple(
            bounty_id,
            "Research cats".into(),
            BountyObjective::WorkAtBuilding,
            12,
            vec![],
        );
        bounty.claimed_by = Some(agent);
        bounty.state = BountyState::PendingVerification;
        bounty.picked_up_tick = Some(0);
        store.tokens.insert(bounty_id, bounty);
        dropbox.deposit_token(agent, bounty_id, None);
        dropbox.deposit_document(
            agent,
            "cats.md".into(),
            "# Cats\nThey purr.".into(),
            None,
        );
    }

    app.world_mut()
        .resource_mut::<PendingVerdicts>()
        .verdicts
        .push((bounty_id, true, "The submitted research matches the bounty.".into()));

    app.update();

    let prompt = prompt_rx
        .try_recv()
        .expect("GM approval should be sent directly to the agent");
    assert!(prompt.contains("BOUNTY APPROVED"));
    assert!(prompt.contains("Research cats"));

    let logs: Vec<_> = app.world().resource::<AgentEventLog>().entries.iter().cloned().collect();
    assert!(logs.iter().any(|entry| {
        entry.kind == LogKind::GmVerdict
            && entry.text.contains("APPROVED Alice Haiku's bounty 'Research cats'")
    }));
    assert!(logs.iter().any(|entry| {
        entry.kind == LogKind::Speech
            && entry.agent == "SYSTEM"
            && entry
                .text
                .contains("[to Alice Haiku] BOUNTY APPROVED by the Game Master!")
    }));

    let mut query = app.world_mut().query::<(&BountyTokenStore, &BountyDropbox)>();
    let (store, dropbox) = query
        .single(app.world())
        .expect("board store/dropbox present");
    let bounty = store.tokens.get(&bounty_id).expect("bounty should exist");
    assert_eq!(bounty.state, BountyState::Completed);
    assert!(dropbox.get_slot(agent).is_none());

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::GoldCoin),
        12
    );
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("GM approved")
    );

    let library = app.world().resource::<Library>();
    assert_eq!(library.documents.len(), 1);
    assert_eq!(library.documents[0].title, "cats.md");
    assert_eq!(library.documents[0].author, "Alice Haiku");
}

#[test]
fn gm_rejection_sends_direct_message_logs_and_returns_document_submission() {
    let mut app = gm_test_app();
    let agent = spawn_gm_test_agent(&mut app, "Bob Sonnet");
    let mut prompt_rx = attach_test_session(&mut app, agent);
    let bounty_id = Uuid::new_v4();

    {
        let mut query = app
            .world_mut()
            .query::<(&mut BountyTokenStore, &mut BountyDropbox)>();
        let (mut store, mut dropbox) = query
            .single_mut(app.world_mut())
            .expect("board store/dropbox present");
        let mut bounty = Bounty::simple(
            bounty_id,
            "Write a market report".into(),
            BountyObjective::WorkAtBuilding,
            9,
            vec![],
        );
        bounty.claimed_by = Some(agent);
        bounty.state = BountyState::PendingVerification;
        bounty.picked_up_tick = Some(0);
        store.tokens.insert(bounty_id, bounty);
        dropbox.deposit_token(agent, bounty_id, None);
        dropbox.deposit_document(
            agent,
            "market_report.md".into(),
            "I need clarification before I can write this.".into(),
            None,
        );
    }

    app.world_mut()
        .resource_mut::<PendingVerdicts>()
        .verdicts
        .push((bounty_id, false, "This is a clarification request, not a report.".into()));

    app.update();

    let prompt = prompt_rx
        .try_recv()
        .expect("GM rejection should be sent directly to the agent");
    assert!(prompt.contains("BOUNTY REJECTED"));
    assert!(prompt.contains("clarification request"));

    let logs: Vec<_> = app.world().resource::<AgentEventLog>().entries.iter().cloned().collect();
    assert!(logs.iter().any(|entry| {
        entry.kind == LogKind::GmVerdict
            && entry
                .text
                .contains("REJECTED Bob Sonnet's bounty 'Write a market report'")
    }));
    assert!(logs.iter().any(|entry| {
        entry.kind == LogKind::Speech
            && entry.agent == "SYSTEM"
            && entry
                .text
                .contains("[to Bob Sonnet] BOUNTY REJECTED by the Game Master.")
    }));

    {
        let mut query = app.world_mut().query::<(&BountyTokenStore, &BountyDropbox)>();
        let (store, dropbox) = query
            .single(app.world())
            .expect("board store/dropbox present");
        let bounty = store.tokens.get(&bounty_id).expect("bounty should exist");
        assert_eq!(bounty.state, BountyState::Claimed);
        assert!(dropbox.get_slot(agent).is_none());
    }

    app.update();

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::BountyToken),
        1
    );
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Document),
        1
    );
    assert!(
        entity
            .get::<DocumentInventory>()
            .expect("docs present")
            .documents
            .contains_key("market_report.md")
    );
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("GM rejected bounty")
    );
}

#[test]
fn gm_rejection_returns_item_proof_submission_for_retry() {
    let mut app = gm_test_app();
    let agent = spawn_gm_test_agent(&mut app, "Carol Opus");
    let mut prompt_rx = attach_test_session(&mut app, agent);
    let bounty_id = Uuid::new_v4();

    {
        let mut entity = app.world_mut().entity_mut(agent);
        entity
            .get_mut::<Inventory>()
            .expect("inventory present")
            .add(ItemType::Coffee, 1);
        entity
            .get_mut::<Inventory>()
            .expect("inventory present")
            .add(ItemType::BountyToken, 1);
    }

    {
        let mut query = app
            .world_mut()
            .query::<(&mut BountyTokenStore, &mut BountyDropbox)>();
        let (mut store, mut dropbox) = query
            .single_mut(app.world_mut())
            .expect("board store/dropbox present");
        let mut bounty = Bounty::simple(
            bounty_id,
            "Deliver coffee proof".into(),
            BountyObjective::WorkAtBuilding,
            4,
            vec![],
        );
        bounty.claimed_by = Some(agent);
        bounty.state = BountyState::PendingVerification;
        bounty.picked_up_tick = Some(0);
        store.tokens.insert(bounty_id, bounty);
        dropbox.deposit_token(agent, bounty_id, None);
        dropbox.deposit_item(agent, ItemType::Coffee, 1);
    }

    {
        let mut entity = app.world_mut().entity_mut(agent);
        entity
            .get_mut::<Inventory>()
            .expect("inventory present")
            .remove(ItemType::Coffee, 1);
        entity
            .get_mut::<Inventory>()
            .expect("inventory present")
            .remove(ItemType::BountyToken, 1);
    }

    app.world_mut()
        .resource_mut::<PendingVerdicts>()
        .verdicts
        .push((bounty_id, false, "Coffee proof was stale.".into()));

    app.update();

    let prompt = prompt_rx
        .try_recv()
        .expect("GM rejection should be sent directly to the agent");
    assert!(prompt.contains("BOUNTY REJECTED"));
    assert!(prompt.contains("Coffee proof was stale"));

    app.update();

    let entity = app.world().entity(agent);
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::BountyToken),
        1
    );
    assert_eq!(
        entity
            .get::<Inventory>()
            .expect("inventory present")
            .count(ItemType::Coffee),
        1
    );

    let logs: Vec<_> = app.world().resource::<AgentEventLog>().entries.iter().cloned().collect();
    assert!(logs.iter().any(|entry| {
        entry.kind == LogKind::Speech
            && entry
                .text
                .contains("[to Carol Opus] BOUNTY REJECTED by the Game Master.")
    }));
}

#[test]
fn go_to_service_with_entrance_is_navigation_only() {
    let mut app = action_test_app();
    app.add_systems(Update, execution_system);

    let cafe = spawn_structure(&mut app, "cafe", GridPos { x: 4, y: 4 }, None);
    let agent = app
        .world_mut()
        .spawn((
            AgentName("Bob Sonnet".into()),
            GridPos { x: 4, y: 4 },
            Speed(1.0),
            AgentGoal::GoingToService {
                building: cafe,
                service: "entrance".into(),
            },
            ThoughtBubble::default(),
            AgentAnimation::default(),
            Inventory::default(),
            Needs::default(),
            ActionLog::default(),
        ))
        .id();

    app.update();

    let entity = app.world().entity(agent);
    assert!(matches!(entity.get::<AgentGoal>(), Some(AgentGoal::Idle)));
    assert!(
        entity
            .get::<ThoughtBubble>()
            .expect("thought present")
            .0
            .contains("Arrived at cafe")
    );
    assert!(entity.get::<server::agents::actions::ActionTimer>().is_none());
}

#[test]
fn direct_service_action_starts_when_agent_is_at_entrance() {
    let mut app = action_test_app();
    app.add_systems(Update, apply_mcp_actions_system);
    let cafe = spawn_structure(&mut app, "cafe", GridPos { x: 6, y: 6 }, None);

    let agent = app
        .world_mut()
        .spawn((
            AgentName("Alice Haiku".into()),
            GridPos { x: 6, y: 6 },
            Speed(1.0),
            AgentGoal::Idle,
            ThoughtBubble::default(),
            KnownLocations::default(),
            BusinessCards::default(),
            AgentAnimation::default(),
            Inventory::default(),
            Needs::default(),
            ActionLog::default(),
        ))
        .id();

    app.world_mut()
        .resource_mut::<PendingActions>()
        .actions
        .push(MpcAction {
            agent_name: "Alice Haiku".into(),
            agent_id: Uuid::new_v4().to_string(),
            action: "buy_muffin".into(),
            building: None,
            service: None,
            agent_target: None,
            text: None,
            feedback: None,
            x: None,
            y: None,
        });

    app.update();

    let entity = app.world().entity(agent);
    let thought_text = entity
        .get::<ThoughtBubble>()
        .map(|t| t.0.clone())
        .unwrap_or_default();
    let goal_text = entity
        .get::<AgentGoal>()
        .map(|g| format!("{g:?}"))
        .unwrap_or_else(|| "<none>".into());
    let timer = entity
        .get::<server::agents::actions::ActionTimer>()
        .unwrap_or_else(|| {
            panic!(
                "service should start immediately; thought={thought_text:?} goal={goal_text}"
            )
        });
    assert_eq!(timer.action_name, "buy_muffin");
    assert!(matches!(
        entity.get::<AgentGoal>(),
        Some(AgentGoal::PerformingAction)
    ));
    assert!(matches!(entity.get::<InsideBuilding>(), Some(InsideBuilding(b)) if *b == cafe));
}
