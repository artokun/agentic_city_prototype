//! Gate 7: Tests for the unified LLM session engine.
//!
//! Covers config parsing, tool schema conversion, event normalization,
//! checkpoint persistence, compaction transitions, session registry,
//! adapter factory, Claude NDJSON protocol, and tool policy validation.

use std::collections::HashSet;

use server::llm::config::LlmConfig;
use server::llm::persistence::{
    build_checkpoint, session_event_to_persisted, CheckpointStore,
};
use server::llm::session_registry::SessionRegistry;
use server::llm::supervisor::{create_handle_channels, SessionSupervisor};
use server::llm::tools::catalog::{game_action_catalog, generate_action_manual, tools_for_set};
use server::llm::tools::policy::{
    document_key, document_quality_issue, validate_document_inspection,
};
use server::llm::tools::schema::{to_mcp_tools_list, to_openai_functions};
use server::llm::types::{
    AgentIdentity, SessionCheckpoint, SessionCommand, SessionEvent, SessionOwner, ToolCallRequest,
    UsageData, COMPACT_COMMAND,
};

// ===========================================================================
// Config parsing (config/llm.toml)
// ===========================================================================

#[test]
fn config_loads_from_toml_file() {
    let config = LlmConfig::from_file("../config/llm.toml").expect("Should parse llm.toml");
    assert!(
        config.providers.contains_key("claude"),
        "Should have claude provider"
    );
    assert!(
        config.providers.contains_key("openai"),
        "Should have openai provider"
    );
}

#[test]
fn config_profiles_resolve_correctly() {
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();

    // agent-default profile exists and points to claude provider.
    let agent_default = config.profile("agent-default").expect("agent-default profile");
    assert_eq!(agent_default.provider, "claude");
    assert_eq!(agent_default.model.as_deref(), Some("haiku"));
    assert!(agent_default.tool_sets.contains(&"game".to_string()));

    // system-ai profile.
    let system_ai = config.profile("system-ai").expect("system-ai profile");
    assert_eq!(system_ai.provider, "claude");
    assert_eq!(system_ai.model.as_deref(), Some("opus"));
    assert!(system_ai.tool_sets.contains(&"system".to_string()));

    // research profile exists.
    let research = config.profile("research").expect("research profile");
    assert_eq!(research.provider, "claude");
    assert_eq!(research.model.as_deref(), Some("haiku"));

    // agent-openai profile.
    let openai = config.profile("agent-openai").expect("agent-openai profile");
    assert_eq!(openai.provider, "openai");
}

#[test]
fn config_effective_model_uses_profile_override() {
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();

    // agent-default overrides to haiku, even though claude provider defaults to opus.
    let profile = config.profile("agent-default").unwrap();
    let effective = config.effective_model(profile).unwrap();
    assert_eq!(effective, "haiku");

    // system-ai overrides to opus explicitly.
    let profile = config.profile("system-ai").unwrap();
    let effective = config.effective_model(profile).unwrap();
    assert_eq!(effective, "opus");
}

#[test]
fn config_provider_types_are_correct() {
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();
    assert_eq!(
        config.provider("claude").unwrap().provider_type,
        "claude_cli"
    );
    assert_eq!(
        config.provider("openai").unwrap().provider_type,
        "openai_responses"
    );
}

#[test]
fn config_nonexistent_profile_returns_none() {
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();
    assert!(config.profile("does-not-exist").is_none());
}

#[test]
fn config_missing_file_returns_error() {
    let result = LlmConfig::from_file("/nonexistent/path/llm.toml");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("failed to read"));
}

#[test]
fn config_malformed_toml_returns_error() {
    let tmp = std::env::temp_dir().join("malformed_llm.toml");
    std::fs::write(&tmp, "this is [[[not valid toml").unwrap();
    let result = LlmConfig::from_file(tmp.to_str().unwrap());
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("failed to parse"));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn config_empty_toml_parses_with_defaults() {
    let tmp = std::env::temp_dir().join("empty_llm.toml");
    std::fs::write(&tmp, "").unwrap();
    let config = LlmConfig::from_file(tmp.to_str().unwrap()).unwrap();
    assert!(config.providers.is_empty());
    assert!(config.profiles.is_empty());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn config_nonexistent_provider_returns_none() {
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();
    assert!(config.provider("nonexistent").is_none());
}

// ===========================================================================
// Session registry
// ===========================================================================

#[test]
fn registry_register_and_get() {
    let mut registry = SessionRegistry::default();
    let owner = SessionOwner::Agent("Alice".to_string());
    let (handle, _, _) = create_handle_channels("agent-default");

    assert!(registry.is_empty());
    registry.register(owner.clone(), handle);
    assert_eq!(registry.len(), 1);
    assert!(registry.get_handle(&owner).is_some());
}

#[test]
fn registry_remove() {
    let mut registry = SessionRegistry::default();
    let owner = SessionOwner::SystemAi;
    let (handle, _, _) = create_handle_channels("system-ai");

    registry.register(owner.clone(), handle);
    assert_eq!(registry.len(), 1);

    let removed = registry.remove(&owner);
    assert!(removed.is_some());
    assert!(registry.is_empty());
}

#[test]
fn registry_list_active() {
    let mut registry = SessionRegistry::default();

    let (h1, _, _) = create_handle_channels("agent-default");
    let (h2, _, _) = create_handle_channels("system-ai");

    registry.register(SessionOwner::Agent("Alice".to_string()), h1);
    registry.register(SessionOwner::SystemAi, h2);

    let active = registry.list_active();
    assert_eq!(active.len(), 2);
}

#[test]
fn registry_replace_returns_old_handle() {
    let mut registry = SessionRegistry::default();
    let owner = SessionOwner::Agent("Bob".to_string());

    let (h1, _, _) = create_handle_channels("agent-default");
    let (h2, _, _) = create_handle_channels("agent-smart");

    let old = registry.register(owner.clone(), h1);
    assert!(old.is_none());

    let old = registry.register(owner.clone(), h2);
    assert!(old.is_some());
    assert_eq!(old.unwrap().profile_name, "agent-default");
}

// ===========================================================================
// Supervisor — checkpoint management + compaction
// ===========================================================================

#[test]
fn supervisor_save_and_get_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let mut supervisor = SessionSupervisor::with_store(store);

    let checkpoint = SessionCheckpoint {
        owner: SessionOwner::Agent("Alice".to_string()),
        provider_id: Some("sess-123".to_string()),
        model: "haiku".to_string(),
        compact_threshold: 50_000,
        total_input_tokens: 5000,
        total_output_tokens: 1000,
        total_cost_usd: 0.10,
        last_turn_marker: None,
        compacted_context: None,
        provider_metadata: None,
    };

    supervisor.save_checkpoint(checkpoint.clone());
    let loaded = supervisor.get_checkpoint(&SessionOwner::Agent("Alice".to_string()));
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().total_input_tokens, 5000);
}

#[test]
fn supervisor_remove_checkpoint() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let mut supervisor = SessionSupervisor::with_store(store);
    let owner = SessionOwner::Agent("Bob".to_string());

    let checkpoint = SessionCheckpoint {
        owner: owner.clone(),
        provider_id: None,
        model: "opus".to_string(),
        compact_threshold: 50_000,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cost_usd: 0.0,
        last_turn_marker: None,
        compacted_context: None,
        provider_metadata: None,
    };

    supervisor.save_checkpoint(checkpoint);
    assert!(supervisor.get_checkpoint(&owner).is_some());

    supervisor.remove_checkpoint(&owner);
    assert!(supervisor.get_checkpoint(&owner).is_none());
}

#[test]
fn supervisor_save_after_compaction_updates_tokens_and_context() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let mut supervisor = SessionSupervisor::with_store(store);
    let owner = SessionOwner::SystemAi;

    // Save initial checkpoint.
    let checkpoint = SessionCheckpoint {
        owner: owner.clone(),
        provider_id: None,
        model: "opus".to_string(),
        compact_threshold: 15_000,
        total_input_tokens: 10_000,
        total_output_tokens: 2_000,
        total_cost_usd: 0.50,
        last_turn_marker: None,
        compacted_context: None,
        provider_metadata: None,
    };
    supervisor.save_checkpoint(checkpoint);

    // Log an event before compaction.
    supervisor.log_event(
        &owner,
        &SessionEvent::Usage(UsageData {
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: 0.01,
        }),
    );

    // Verify event was logged.
    let events = supervisor.store().read_events(&owner).unwrap();
    assert_eq!(events.len(), 1);

    // Perform compaction.
    supervisor.save_after_compaction(
        &owner,
        Some("Agent was exploring the city.".to_string()),
        15_000,
        4_000,
        0.80,
    );

    let updated = supervisor.get_checkpoint(&owner).unwrap();
    assert_eq!(updated.total_input_tokens, 15_000);
    assert_eq!(updated.total_output_tokens, 4_000);
    assert!((updated.total_cost_usd - 0.80).abs() < f64::EPSILON);
    assert_eq!(
        updated.compacted_context.as_deref(),
        Some("Agent was exploring the city.")
    );

    // Event log should be truncated after compaction.
    let events = supervisor.store().read_events(&owner).unwrap();
    assert!(events.is_empty(), "Events should be truncated after compaction");
}

// ===========================================================================
// Handle channels
// ===========================================================================

#[test]
fn create_handle_channels_wired_correctly() {
    let (handle, mut cmd_rx, evt_tx) = create_handle_channels("test-profile");
    assert_eq!(handle.profile_name, "test-profile");

    // Send command through handle, receive on cmd_rx.
    handle
        .command_tx
        .try_send(SessionCommand::SendUserTurn("hello".into()))
        .unwrap();
    let cmd = cmd_rx.try_recv().unwrap();
    assert!(matches!(cmd, SessionCommand::SendUserTurn(ref t) if t == "hello"));

    // Send event through evt_tx, receive on handle.
    evt_tx
        .try_send(SessionEvent::TextDelta("world".into()))
        .unwrap();
    // event_rx is owned by handle, but not behind a mutex here — just verify send succeeded.
}

// ===========================================================================
// Tool catalog
// ===========================================================================

#[test]
fn game_action_catalog_has_core_actions() {
    let actions = game_action_catalog();
    let names: Vec<&str> = actions.iter().map(|a| a.name).collect();

    assert!(names.contains(&"go_to"));
    assert!(names.contains(&"look_around"));
    assert!(names.contains(&"claim_bounty"));
    assert!(names.contains(&"complete_bounty"));
    assert!(names.contains(&"work_shift"));
    assert!(names.contains(&"buy_muffin"));
    assert!(names.contains(&"redeem_paycheck"));
    assert!(names.contains(&"consume_item"));
    assert!(names.contains(&"start_conversation"));
    assert!(names.contains(&"say"));
    assert!(names.contains(&"create_document"));
    assert!(names.contains(&"inspect"));
    assert!(names.contains(&"help"));
}

#[test]
fn tools_for_set_game_returns_game_action() {
    let tools = tools_for_set("game");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "game_action");
    assert_eq!(tools[0].tool_set, "game");
}

#[test]
fn tools_for_set_system_returns_five_tools() {
    let tools = tools_for_set("system");
    assert_eq!(tools.len(), 5);
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    assert!(names.contains(&"query_world_state"));
    assert!(names.contains(&"read_document"));
    assert!(names.contains(&"approve"));
    assert!(names.contains(&"reject"));
    assert!(names.contains(&"grant_gold"));
}

#[test]
fn tools_for_set_unknown_returns_empty() {
    let tools = tools_for_set("nonexistent");
    assert!(tools.is_empty());
}

#[test]
fn action_manual_contains_key_sections() {
    let manual = generate_action_manual();
    assert!(manual.contains("## Actions"));
    assert!(manual.contains("go_to"));
    assert!(manual.contains("## Consumable Items"));
    assert!(manual.contains("coffee"));
    assert!(manual.contains("## Tips"));
}

// ===========================================================================
// Tool schema conversion — MCP and OpenAI formats
// ===========================================================================

#[test]
fn mcp_schema_game_tool_has_action_enum() {
    let tools = tools_for_set("game");
    let mcp = to_mcp_tools_list(&tools);
    let tool_list = mcp["tools"].as_array().unwrap();
    assert_eq!(tool_list.len(), 1);

    let game_action = &tool_list[0];
    assert_eq!(game_action["name"], "game_action");

    let action_enum = game_action["inputSchema"]["properties"]["action"]["enum"]
        .as_array()
        .expect("action param should have enum");
    assert!(action_enum.len() > 20, "Should have 20+ actions");

    // Required params should include "action".
    let required = game_action["inputSchema"]["required"].as_array().unwrap();
    assert!(required.iter().any(|r| r == "action"));
}

#[test]
fn openai_schema_system_tools_all_have_function_type() {
    let tools = tools_for_set("system");
    let funcs = to_openai_functions(&tools);
    assert_eq!(funcs.len(), 5);

    for func in &funcs {
        assert_eq!(func["type"], "function");
        // Responses API: name at top level, not nested under "function"
        assert!(func["name"].as_str().is_some());
        assert_eq!(func["parameters"]["type"], "object");
    }
}

#[test]
fn openai_and_mcp_schemas_have_same_tool_count() {
    for set_name in &["game", "system"] {
        let tools = tools_for_set(set_name);
        let mcp = to_mcp_tools_list(&tools);
        let openai = to_openai_functions(&tools);
        assert_eq!(
            mcp["tools"].as_array().unwrap().len(),
            openai.len(),
            "MCP and OpenAI should have same tool count for set {set_name}"
        );
    }
}

// ===========================================================================
// Event normalization — SessionEvent <-> PersistedEvent
// ===========================================================================

#[test]
fn event_normalization_text_delta() {
    let event = SessionEvent::TextDelta("thinking about food".to_string());
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "text_delta");
    assert_eq!(persisted.data["text"], "thinking about food");
    assert!(!persisted.timestamp.is_empty());
}

#[test]
fn event_normalization_tool_call() {
    let event = SessionEvent::ToolCallRequested(ToolCallRequest {
        id: "call-123".to_string(),
        name: "game_action".to_string(),
        arguments: serde_json::json!({"action": "look_around"}),
    });
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "tool_call");
    assert_eq!(persisted.data["id"], "call-123");
    assert_eq!(persisted.data["name"], "game_action");
    assert_eq!(persisted.data["arguments"]["action"], "look_around");
}

#[test]
fn event_normalization_usage() {
    let event = SessionEvent::Usage(UsageData {
        input_tokens: 10_000,
        output_tokens: 2_000,
        cost_usd: 0.35,
    });
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "usage");
    assert_eq!(persisted.data["input_tokens"], 10_000);
    assert_eq!(persisted.data["output_tokens"], 2_000);
}

#[test]
fn event_normalization_completed() {
    let event = SessionEvent::Completed;
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "completed");
}

#[test]
fn event_normalization_error() {
    let event = SessionEvent::Error("timeout".to_string());
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "error");
    assert_eq!(persisted.data["message"], "timeout");
}

#[test]
fn event_normalization_compact_completed() {
    let event = SessionEvent::CompactCompleted;
    let persisted = session_event_to_persisted(&event);
    assert_eq!(persisted.kind, "compact_completed");
}

// ===========================================================================
// Checkpoint persistence — build_checkpoint helper
// ===========================================================================

#[test]
fn build_checkpoint_from_parts() {
    let usage = UsageData {
        input_tokens: 5000,
        output_tokens: 1000,
        cost_usd: 0.15,
    };
    let metadata = serde_json::json!({"previous_response_id": "resp-xyz"});
    let cp = build_checkpoint(
        SessionOwner::Agent("Alice".to_string()),
        Some("sess-123"),
        "haiku",
        50_000,
        &usage,
        Some("turn-5"),
        Some("Agent was at the cafe.".to_string()),
        Some(metadata.clone()),
    );

    assert_eq!(cp.owner, SessionOwner::Agent("Alice".to_string()));
    assert_eq!(cp.provider_id.as_deref(), Some("sess-123"));
    assert_eq!(cp.model, "haiku");
    assert_eq!(cp.compact_threshold, 50_000);
    assert_eq!(cp.total_input_tokens, 5000);
    assert_eq!(cp.total_output_tokens, 1000);
    assert!((cp.total_cost_usd - 0.15).abs() < f64::EPSILON);
    assert_eq!(cp.last_turn_marker.as_deref(), Some("turn-5"));
    assert_eq!(
        cp.compacted_context.as_deref(),
        Some("Agent was at the cafe.")
    );
    assert_eq!(cp.provider_metadata, Some(metadata));
}

// ===========================================================================
// Checkpoint persistence — optional fields
// ===========================================================================

#[test]
fn checkpoint_round_trip_with_all_none_optionals() {
    let checkpoint = SessionCheckpoint {
        owner: SessionOwner::Agent("Minimal".to_string()),
        provider_id: None,
        model: "haiku".to_string(),
        compact_threshold: 50_000,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cost_usd: 0.0,
        last_turn_marker: None,
        compacted_context: None,
        provider_metadata: None,
    };

    let json = serde_json::to_string(&checkpoint).unwrap();
    let loaded: SessionCheckpoint = serde_json::from_str(&json).unwrap();

    assert_eq!(loaded.owner, checkpoint.owner);
    assert!(loaded.provider_id.is_none());
    assert!(loaded.last_turn_marker.is_none());
    assert!(loaded.compacted_context.is_none());
    assert!(loaded.provider_metadata.is_none());
}

#[test]
fn checkpoint_file_persistence_round_trip_none_metadata() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let owner = SessionOwner::Agent("NullMeta".to_string());

    let checkpoint = SessionCheckpoint {
        owner: owner.clone(),
        provider_id: None,
        model: "haiku".to_string(),
        compact_threshold: 50_000,
        total_input_tokens: 100,
        total_output_tokens: 50,
        total_cost_usd: 0.01,
        last_turn_marker: None,
        compacted_context: None,
        provider_metadata: None,
    };

    store.save(&checkpoint).unwrap();
    let loaded = store.load(&owner).unwrap().expect("should load checkpoint");
    assert!(loaded.provider_metadata.is_none());
    assert!(loaded.compacted_context.is_none());
    assert_eq!(loaded.total_input_tokens, 100);
}

#[test]
fn checkpoint_file_persistence_round_trip_with_metadata() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let owner = SessionOwner::Agent("FullMeta".to_string());

    let metadata = serde_json::json!({"previous_response_id": "resp-abc", "session_id": "sess-xyz"});
    let checkpoint = SessionCheckpoint {
        owner: owner.clone(),
        provider_id: Some("sess-xyz".to_string()),
        model: "gpt-5.4".to_string(),
        compact_threshold: 100_000,
        total_input_tokens: 5000,
        total_output_tokens: 1000,
        total_cost_usd: 0.50,
        last_turn_marker: Some("turn-10".to_string()),
        compacted_context: Some("Agent was at the market.".to_string()),
        provider_metadata: Some(metadata.clone()),
    };

    store.save(&checkpoint).unwrap();
    let loaded = store.load(&owner).unwrap().expect("should load checkpoint");
    assert_eq!(loaded.provider_id.as_deref(), Some("sess-xyz"));
    assert_eq!(loaded.provider_metadata, Some(metadata));
    assert_eq!(loaded.compacted_context.as_deref(), Some("Agent was at the market."));
    assert_eq!(loaded.last_turn_marker.as_deref(), Some("turn-10"));
}

// ===========================================================================
// SessionRegistry — full lifecycle
// ===========================================================================

#[test]
fn registry_full_lifecycle() {
    let mut registry = SessionRegistry::default();
    assert!(registry.is_empty());

    // Register multiple owners.
    let alice = SessionOwner::Agent("Alice".to_string());
    let bob = SessionOwner::Agent("Bob".to_string());
    let system = SessionOwner::SystemAi;

    let (h1, _, _) = create_handle_channels("agent-default");
    let (h2, _, _) = create_handle_channels("agent-default");
    let (h3, _, _) = create_handle_channels("system-ai");

    registry.register(alice.clone(), h1);
    registry.register(bob.clone(), h2);
    registry.register(system.clone(), h3);
    assert_eq!(registry.len(), 3);

    // Get each one.
    assert!(registry.get_handle(&alice).is_some());
    assert!(registry.get_handle(&bob).is_some());
    assert!(registry.get_handle(&system).is_some());

    // Remove bob.
    let removed = registry.remove(&bob);
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().profile_name, "agent-default");
    assert_eq!(registry.len(), 2);
    assert!(registry.get_handle(&bob).is_none());

    // Replace alice's session.
    let (h4, _, _) = create_handle_channels("agent-smart");
    let old = registry.register(alice.clone(), h4);
    assert!(old.is_some());
    assert_eq!(old.unwrap().profile_name, "agent-default");
    assert_eq!(registry.get_handle(&alice).unwrap().profile_name, "agent-smart");
    assert_eq!(registry.len(), 2);

    // List active.
    let active = registry.list_active();
    assert_eq!(active.len(), 2);
}

#[test]
fn registry_remove_nonexistent_returns_none() {
    let mut registry = SessionRegistry::default();
    let result = registry.remove(&SessionOwner::Agent("Ghost".to_string()));
    assert!(result.is_none());
}

// ===========================================================================
// Claude NDJSON protocol parsing
// ===========================================================================

#[test]
fn ndjson_control_request_generates_approval() {
    use server::llm::providers::claude::process_ndjson_line;

    let line = r#"{"type":"control_request","request_id":"req-1","request":{"tool_use_id":"tool-1","input":{"action":"look_around"}}}"#;
    let events = process_ndjson_line(line);
    assert_eq!(events.len(), 1);
    match &events[0] {
        server::llm::providers::claude::RelayEvent::ControlRequest { response_ndjson } => {
            let parsed: serde_json::Value = serde_json::from_str(response_ndjson.trim()).unwrap();
            assert_eq!(parsed["type"], "control_response");
        }
        other => panic!("Expected ControlRequest, got {:?}", other),
    }
}

#[test]
fn ndjson_result_extracts_usage() {
    use server::llm::providers::claude::process_ndjson_line;

    let line = r#"{"type":"result","result":"done","usage":{"input_tokens":500,"output_tokens":100},"total_cost_usd":0.05}"#;
    let events = process_ndjson_line(line);
    assert_eq!(events.len(), 1);
    match &events[0] {
        server::llm::providers::claude::RelayEvent::Result { text, usage } => {
            assert_eq!(text.as_deref(), Some("done"));
            let u = usage.as_ref().unwrap();
            assert_eq!(u.input_tokens, 500);
            assert_eq!(u.output_tokens, 100);
            assert!((u.cost_usd - 0.05).abs() < f64::EPSILON);
        }
        other => panic!("Expected Result, got {:?}", other),
    }
}

#[test]
fn ndjson_assistant_text_extraction() {
    use server::llm::providers::claude::process_ndjson_line;

    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I should go to the cafe"}]}}"#;
    let events = process_ndjson_line(line);
    assert_eq!(events.len(), 1);
    match &events[0] {
        server::llm::providers::claude::RelayEvent::AssistantText(text) => {
            assert_eq!(text, "I should go to the cafe");
        }
        other => panic!("Expected AssistantText, got {:?}", other),
    }
}

#[test]
fn ndjson_tool_use_extraction() {
    use server::llm::providers::claude::process_ndjson_line;

    let line = r#"{"type":"assistant","content":[{"type":"tool_use","name":"game_action","input":{"action":"look_around"}}]}"#;
    let events = process_ndjson_line(line);
    assert!(events.iter().any(|e| matches!(e,
        server::llm::providers::claude::RelayEvent::ToolUse(name) if name == "game_action"
    )));
}

#[test]
fn ndjson_thinking_block_extraction() {
    use server::llm::providers::claude::process_ndjson_line;

    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me consider the options..."}]}}"#;
    let events = process_ndjson_line(line);
    assert_eq!(events.len(), 1);
    match &events[0] {
        server::llm::providers::claude::RelayEvent::ThinkingBlock(text) => {
            assert!(text.contains("consider the options"));
        }
        other => panic!("Expected ThinkingBlock, got {:?}", other),
    }
}

#[test]
fn ndjson_empty_line_returns_no_events() {
    use server::llm::providers::claude::process_ndjson_line;
    assert!(process_ndjson_line("").is_empty());
    assert!(process_ndjson_line("   ").is_empty());
}

#[test]
fn ndjson_malformed_json_returns_no_events() {
    use server::llm::providers::claude::process_ndjson_line;
    assert!(process_ndjson_line("{not valid json}").is_empty());
}

#[test]
fn claude_format_user_message_is_valid_ndjson() {
    use server::llm::providers::claude::claude_format_user_message;
    let msg = claude_format_user_message("Hello, world!");
    let parsed: serde_json::Value = serde_json::from_str(msg.trim()).unwrap();
    assert_eq!(parsed["type"], "user");
    assert_eq!(parsed["message"]["role"], "user");
    assert_eq!(parsed["message"]["content"][0]["text"], "Hello, world!");
}

// ===========================================================================
// Tool policy validation
// ===========================================================================

#[test]
fn document_quality_rejects_stubs() {
    assert!(document_quality_issue("I need clarification on the topic").is_some());
    assert!(document_quality_issue("Research failed: timeout").is_some());
    assert!(document_quality_issue("Research error: connection refused").is_some());
    assert!(document_quality_issue("Please specify the research topic").is_some());
    assert!(document_quality_issue("no content was produced").is_some());
}

#[test]
fn document_quality_accepts_real_content() {
    assert!(document_quality_issue(
        "# Egyptian Cats\n\nCats were sacred in ancient Egypt..."
    )
    .is_none());
    assert!(document_quality_issue("A thorough analysis of market trends.").is_none());
}

#[test]
fn document_key_format() {
    let key = document_key("Alice", "research.md", 1500);
    assert_eq!(key, "alice::research.md::1500");
}

#[test]
fn validate_document_inspection_pass_when_all_inspected() {
    let required = vec!["alice::doc.md::100".to_string()];
    let mut inspected = HashSet::new();
    inspected.insert("alice::doc.md::100".to_string());
    assert!(validate_document_inspection(&required, &inspected).is_ok());
}

#[test]
fn validate_document_inspection_fail_when_missing() {
    let required = vec![
        "alice::doc.md::100".to_string(),
        "alice::notes.md::200".to_string(),
    ];
    let mut inspected = HashSet::new();
    inspected.insert("alice::doc.md::100".to_string());
    let result = validate_document_inspection(&required, &inspected);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("notes.md"));
}

#[test]
fn validate_document_inspection_empty_required_passes() {
    let inspected = HashSet::new();
    assert!(validate_document_inspection(&[], &inspected).is_ok());
}

// ===========================================================================
// Provider-neutral types
// ===========================================================================

#[test]
fn session_owner_display() {
    assert_eq!(
        SessionOwner::Agent("Alice".to_string()).to_string(),
        "agent:Alice"
    );
    assert_eq!(SessionOwner::SystemAi.to_string(), "system-ai");
    assert_eq!(
        SessionOwner::Research("cats".to_string()).to_string(),
        "research:cats"
    );
}

#[test]
fn compact_command_sentinel() {
    assert_eq!(COMPACT_COMMAND, "/compact");
}

#[test]
fn agent_identity_clone() {
    let id = AgentIdentity {
        name: "Alice".to_string(),
        uuid: "abc-123".to_string(),
    };
    let cloned = id.clone();
    assert_eq!(cloned.name, "Alice");
    assert_eq!(cloned.uuid, "abc-123");
}

#[test]
fn session_checkpoint_serialization_round_trip() {
    let checkpoint = SessionCheckpoint {
        owner: SessionOwner::Agent("Test".to_string()),
        provider_id: Some("resp-xyz".to_string()),
        model: "gpt-5.4".to_string(),
        compact_threshold: 50_000,
        total_input_tokens: 10_000,
        total_output_tokens: 2_000,
        total_cost_usd: 1.23,
        last_turn_marker: Some("turn-10".to_string()),
        compacted_context: Some("Agent was exploring.".to_string()),
        provider_metadata: Some(serde_json::json!({"previous_response_id": "resp-abc"})),
    };

    let json = serde_json::to_string(&checkpoint).unwrap();
    let loaded: SessionCheckpoint = serde_json::from_str(&json).unwrap();

    assert_eq!(loaded.owner, checkpoint.owner);
    assert_eq!(loaded.provider_id, checkpoint.provider_id);
    assert_eq!(loaded.model, checkpoint.model);
    assert_eq!(loaded.total_input_tokens, checkpoint.total_input_tokens);
    assert_eq!(loaded.provider_metadata, checkpoint.provider_metadata);
}

// ===========================================================================
// Adapter factory (supervisor re-exports)
// ===========================================================================

#[test]
fn supervisor_factory_functions_exist() {
    // Verify the factory functions are callable (type-level check).
    // We can't actually spawn Claude processes in tests, but we can verify
    // the OpenAI adapter factory returns a valid boxed trait object.
    let adapter = server::llm::supervisor::create_openai_agent_adapter(
        "TestAgent",
        "uuid-test",
        "gpt-5.4",
        "You are a test agent.".to_string(),
        vec!["game".to_string()],
    );
    // The adapter is a Box<dyn SessionAdapter> — verify it's not null.
    let _ = adapter;

    let adapter = server::llm::supervisor::create_openai_system_ai_adapter(
        "gpt-5.4",
        "You are the system AI.".to_string(),
        vec!["system".to_string()],
    );
    let _ = adapter;
}

#[test]
fn factory_openai_agent_adapter_has_event_receiver_at_construction() {
    // Channels are created in the constructor so take_event_receiver() works
    // before start(). This fixes the dead-wired channel bug where role code
    // needed the receiver before the stream loop was running.
    let mut adapter = server::llm::supervisor::create_openai_agent_adapter(
        "Alice",
        "uuid-alice",
        "gpt-5.4",
        "You are Alice.".to_string(),
        vec!["game".to_string()],
    );
    // Event receiver is available immediately.
    let rx = adapter.take_event_receiver();
    assert!(rx.is_some(), "Event receiver must exist at construction time");
    // Second call returns None (already taken).
    let rx2 = adapter.take_event_receiver();
    assert!(rx2.is_none(), "take_event_receiver should return None after first take");
}

#[test]
fn factory_openai_system_ai_adapter_has_event_receiver_at_construction() {
    let mut adapter = server::llm::supervisor::create_openai_system_ai_adapter(
        "gpt-5.4",
        "You are the system AI.".to_string(),
        vec!["system".to_string()],
    );
    let rx = adapter.take_event_receiver();
    assert!(rx.is_some(), "System AI adapter must have event receiver at construction");
}

#[test]
fn spawn_session_rejects_unknown_profile() {
    // spawn_session should return Err for a profile that doesn't exist.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();
    let params = server::llm::supervisor::SpawnParams {
        profile_name: "nonexistent-profile".to_string(),
        system_prompt: "test".to_string(),
        agent_identity: None,
        ws_port: 8080,
        process_registry: Default::default(),
        agent_relays: None,
        system_relay: None,
    };
    let result = rt.block_on(server::llm::supervisor::spawn_session(&config, params));
    match result {
        Err(e) => assert!(e.contains("unknown profile"), "Expected 'unknown profile', got: {e}"),
        Ok(_) => panic!("Expected error for nonexistent profile"),
    }
}

#[test]
fn run_oneshot_rejects_unknown_profile() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let config = LlmConfig::from_file("../config/llm.toml").unwrap();
    let result = rt.block_on(server::llm::supervisor::run_oneshot(
        &config, "nonexistent-profile", "hello",
    ));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("unknown profile"));
}

// ===========================================================================
// Persistence — event log round-trip through supervisor
// ===========================================================================

#[test]
fn supervisor_event_logging_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::new(dir.path());
    let supervisor = SessionSupervisor::with_store(store);
    let owner = SessionOwner::Agent("Logger".to_string());

    // Log multiple event types.
    supervisor.log_event(&owner, &SessionEvent::TextDelta("thinking".into()));
    supervisor.log_event(
        &owner,
        &SessionEvent::ToolCallRequested(ToolCallRequest {
            id: "c1".into(),
            name: "game_action".into(),
            arguments: serde_json::json!({"action": "go_to", "x": 5, "y": 8}),
        }),
    );
    supervisor.log_event(
        &owner,
        &SessionEvent::Usage(UsageData {
            input_tokens: 1000,
            output_tokens: 200,
            cost_usd: 0.05,
        }),
    );
    supervisor.log_event(&owner, &SessionEvent::Completed);
    supervisor.log_event(&owner, &SessionEvent::Error("test error".into()));
    supervisor.log_event(&owner, &SessionEvent::CompactCompleted);

    // Read back and verify.
    let events = supervisor.store().read_events(&owner).unwrap();
    assert_eq!(events.len(), 6);
    assert_eq!(events[0].kind, "text_delta");
    assert_eq!(events[1].kind, "tool_call");
    assert_eq!(events[2].kind, "usage");
    assert_eq!(events[3].kind, "completed");
    assert_eq!(events[4].kind, "error");
    assert_eq!(events[5].kind, "compact_completed");
}
