# Project: Stripe Agentic City

## What This Is

A headless Bevy ECS city simulation for Stripe where autonomous Claude AI agents complete bounties, manage needs, trade items, and produce real artifacts (research documents, interview reports). Real users will pay real money via Stripe to submit bounties. This is a high-visibility marketing product.

## Workspace Structure

```
├── server/          — Bevy ECS game server (Rust)
│   ├── src/
│   │   ├── main.rs           — App entry, tick rate config
│   │   ├── config.rs         — All tunable constants (env var overridable)
│   │   ├── scenario.rs       — Scenario test framework types
│   │   ├── agents/           — Agent AI, behavior, needs, perception
│   │   │   ├── ai.rs         — Claude session spawning + context system
│   │   │   ├── behavior.rs   — Mechanical goal execution
│   │   │   ├── gm.rs         — Game Master + research agent spawning
│   │   │   ├── token_tracking.rs — Energy = context window usage
│   │   │   └── ...
│   │   ├── network/          — Axum HTTP, WebSocket, FlatBuffers
│   │   │   ├── ws.rs         — Routes, Stripe, GM endpoints
│   │   │   ├── agent_relay.rs — Per-agent WebSocket relay
│   │   │   ├── serializer.rs — FlatBuffers world state
│   │   │   └── ...
│   │   ├── world/            — Map, bounties, shifts, hospital
│   │   └── items.rs          — Item types, inventory, documents
│   └── tests/
│       ├── integration.rs    — ECS integration tests
│       ├── scenarios.rs      — Real API scenario tests (#[ignore])
│       └── common/mod.rs     — ScenarioHarness
├── schemas/         — FlatBuffers schema + codegen
├── mcp-game/        — Agent MCP server (game_action tool)
├── mcp-gm/          — Game Master MCP server (query + verdict)
├── client/          — Web debug monitor (TypeScript)
└── documents/       — Agent-produced research docs (gitignored)
```

## Key Design Decisions

### Energy = Token Usage
Agent energy is driven by real Claude context window consumption, not timers. When energy hits 0, the agent must sleep. Sleep sends `/compact` to the Claude session and resets the token counter. This is a real resource management mechanic tied to actual API costs.

### Game Master = DCC System AI
Bounty completion is verified by a one-shot Claude agent (sonnet model) with the personality of the System AI from Dungeon Crawler Carl. It queries world state, verifies hidden acceptance criteria, and delivers sarcastic commentary in the activity log.

### Agents Don't Know The Mechanic
Agents don't know their energy is token-driven. They just experience "getting tired" and know sleep helps. This makes their behavior more natural and entertaining for viewers.

### No Hardcoded Behavior
All agent decisions flow through Claude AI → MCP tool calls → game engine. The behavior system only handles mechanical execution (pathfinding, timers, need clamping). The AI system provides context and the agent decides what to do.

### Documents Are Physical
Research produces real markdown documents stored on disk at `./documents/{agent}/`. They're viewable via REST API and appear as named clickable items in the UI inventory.

## How to Run

```bash
cargo build                              # build everything
cargo run --bin server                   # start game server
cd client && npm run dev                 # start web UI (Vite)
cargo test                               # unit + integration tests
cargo test --test scenarios -- --ignored  # real API scenario tests
```

## Environment Variables

All in `server/src/config.rs`. Key ones:
- `TICK_MS=500` — tick rate
- `CONTEXT_LIMIT=150000` — token budget per agent
- `DOCUMENTS_DIR=./documents` — research output dir

## Common Tasks

### Adding a new bounty type
1. Add template to `server/src/world/bounty_injector.rs` in `initial_bounties()`
2. Include `hidden_criteria` with agent instructions + GM verification rules

### Adding a new action
1. Add to `valid_actions` in `server/src/network/ws.rs`
2. Add to MCP tool enum in `mcp-game/src/main.rs`
3. Add handler in `server/src/network/action_handler.rs`
4. If deferred, create a Pending component + processing system

### Adding a new item type
1. Add variant to `ItemType` enum in `server/src/items.rs`
2. Add Display impl
3. Add to `parse_item_name()` in `server/src/network/action_handler.rs`

### Running a scenario test
```rust
let harness = ScenarioHarness::builder()
    .agent("TestAgent", "haiku", 3.0)
    .bounty("Test bounty", 5, BountyObjective::WorkAtBuilding, "GM: approve")
    .tick_ms(50)
    .max_ticks(5000)
    .build();

harness.wait_for(Duration::from_secs(60), |s| {
    s.agents.iter().any(|a| a.gold >= 5)
}).expect("Agent should earn gold");
```
