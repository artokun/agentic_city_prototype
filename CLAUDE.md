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
cargo build                              # build everything (server + mcp-game + mcp-gm)
cargo run --bin server                   # start game server (port 8080)
cd client && npm run dev                 # start web UI (Vite, port 5173)
cargo test                               # unit + integration tests
cargo test --test scenarios -- --ignored  # real API scenario tests
```

### Running with the debug UI

```bash
# Terminal 1: game server
cargo run --bin server

# Terminal 2: web UI (Vite dev server with hot reload)
cd client && npm run dev
# Open http://localhost:5173 in browser
```

### Debug endpoints

```bash
# Full debug feed (all events, newest first)
curl http://localhost:8080/api/debug | python3 -m json.tool

# Filter by agent
curl 'http://localhost:8080/api/debug?agent=Carol&limit=20'

# Filter by event type (thought, action, speech, system, decision, feedback, gm_verdict, gm_thinking, gm_response)
curl 'http://localhost:8080/api/debug?kind=gm_verdict,gm_thinking'

# Text search
curl 'http://localhost:8080/api/debug?q=exploit&agent=Carol'

# Combine filters
curl 'http://localhost:8080/api/debug?agent=Bob&kind=thought,action&since=100&limit=50'

# Library catalog
curl http://localhost:8080/api/library

# Agent documents
curl http://localhost:8080/api/documents
```

## Environment Variables

All in `server/src/config.rs`. Key ones:
- `TICK_MS=1000` — tick rate (1Hz)
- `CONTEXT_LIMIT=50000` — token budget per agent
- `STATUS_INTERVAL=30` — ticks between status updates
- `HUNGER_DECAY=0.333` — hunger drain per tick (~5min to empty)
- `DOCUMENTS_DIR=./documents` — research output dir
- `BOUNTIES_FILE=bounties.json` — initial bounty definitions

## Common Tasks

### Adding a new bounty
1. Edit `bounties.json` at project root — parsed at startup
2. Format: `{ "title", "instructions", "hidden_criteria", "objective", "reward", "claim_items" }`
3. Objectives: `"WorkAtBuilding"`, `"HideItem(gold_egg)"`, `"FindItem(gold_egg)"`

### Adding a new action
1. Add entry to `action_catalog()` in `mcp-game/src/main.rs` — this is the single source of truth
2. Add handler in `server/src/network/action_handler.rs`
3. The MCP schema, game manual, and server whitelist all derive from the catalog automatically
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
