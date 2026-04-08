# Project: Stripe Agentic City

## What This Is

A headless Bevy ECS city simulation for Stripe where autonomous AI agents complete bounties, manage needs, trade items, and produce real artifacts (research documents, interview reports). Supports multiple LLM providers (Claude CLI, OpenAI Responses API) via a unified session engine. Real users will pay real money via Stripe to submit bounties. This is a high-visibility marketing product.

## Workspace Structure

```
├── server/          — Bevy ECS game server (Rust)
│   ├── src/
│   │   ├── main.rs           — App entry, dotenvy, tick rate config
│   │   ├── config.rs         — All tunable constants (env var overridable)
│   │   ├── scenario.rs       — Scenario test framework types
│   │   ├── agents/           — Agent AI, behavior, needs, perception
│   │   │   ├── ai.rs         — Session spawning, context system, spawn backoff
│   │   │   ├── behavior.rs   — Mechanical goal execution
│   │   │   ├── gm.rs         — Persistent Game Master session management
│   │   │   ├── personality.rs — Agent personalities and system prompts
│   │   │   ├── token_tracking.rs — Energy = context window usage
│   │   │   └── ...
│   │   ├── llm/              — Unified LLM session engine
│   │   │   ├── config.rs     — TOML config parsing (providers + profiles)
│   │   │   ├── supervisor.rs — Session lifecycle, factory routing
│   │   │   ├── providers/
│   │   │   │   ├── claude.rs — Claude CLI adapter (--sdk-url, MCP config)
│   │   │   │   └── openai.rs — OpenAI Responses API adapter (WebSocket)
│   │   │   ├── tools/        — Action catalog, schema gen, execution
│   │   │   ├── session_registry.rs — Session lifecycle tracking
│   │   │   └── persistence.rs — Checkpoint/resume (WIP)
│   │   ├── network/          — Axum HTTP, WebSocket, FlatBuffers
│   │   │   ├── ws.rs         — Routes, Stripe, GM endpoints
│   │   │   ├── agent_relay.rs — Per-agent WebSocket relay
│   │   │   ├── system_relay.rs — GM WebSocket relay
│   │   │   ├── action_handler.rs — Game action dispatch + GM verdict processing
│   │   │   ├── serializer.rs — FlatBuffers world state
│   │   │   └── ...
│   │   ├── world/            — Map, bounties, shifts, hospital, economy
│   │   └── items.rs          — Item types, inventory, documents
│   └── tests/
│       ├── integration.rs    — 16 ECS integration tests (GM verdicts, bounties, etc.)
│       ├── llm_session_engine.rs — 61 session engine unit tests
│       ├── scenarios.rs      — Real API scenario tests (#[ignore])
│       └── common/mod.rs     — ScenarioHarness
├── config/          — LLM provider config (llm.toml)
├── schemas/         — FlatBuffers schema + codegen
├── mcp-game/        — Agent MCP server (game_action tool, 38+ actions)
├── mcp-gm/          — Game Master MCP server (query + approve/reject)
├── debug-ui/        — Web debug monitor (TypeScript + Vite)
└── documents/       — Agent-produced research docs (gitignored)
```

## Key Design Decisions

### Multi-Provider LLM Engine
Sessions are configured via `config/llm.toml` with provider + profile abstraction. Providers: `claude_cli` (spawns Claude CLI process with `--sdk-url` WebSocket relay) and `openai_responses` (in-process WebSocket to OpenAI API). Profiles map agent roles to providers, models, and tool sets.

### Spawn Backoff
Failed LLM sessions retry with exponential backoff (10→20→40→80→160→300 tick cap, max 8 retries). Backoff is keyed by agent name to avoid Entity recycling issues. The Game Master panics on max failures since the game cannot function without it.

### Tool Restrictions
Claude agent sessions use `--disallowedTools` to block Bash, Read, Write, Edit, Glob, Grep, Agent, Skill, WebFetch, WebSearch, NotebookEdit. Agents can ONLY use the `mcp__game-engine__game_action` MCP tool. The GM is exempt (needs document inspection).

### Energy = Token Usage
Agent energy is driven by real Claude context window consumption, not timers. When energy hits 0, the agent must sleep. Sleep sends `/compact` to the Claude session and resets the token counter. This is a real resource management mechanic tied to actual API costs.

### Game Master = Persistent System AI
The GM is a persistent Claude session (opus model) with the personality of the System AI from Dungeon Crawler Carl. It queries world state, reads submitted documents, verifies hidden acceptance criteria, and delivers sarcastic verdicts. GM verdicts hit three surfaces:
- Direct message to the agent's session channel
- Activity log as `gm_verdict` event
- Chat-visible log as `speech` from `SYSTEM`

Rejected bounties return to `Claimed` state so agents can retry.

### Agents Don't Know The Mechanic
Agents don't know their energy is token-driven. They just experience "getting tired" and know sleep helps. This makes their behavior more natural and entertaining for viewers.

### No Hardcoded Behavior
All agent decisions flow through LLM AI → MCP tool calls → game engine. The behavior system only handles mechanical execution (pathfinding, timers, need clamping). The AI system provides context and the agent decides what to do.

### Documents Are Physical
Research produces real markdown documents stored on disk at `./documents/{agent}/`. They're viewable via REST API and appear as named clickable items in the UI inventory.

### Bounty Board Is Physical
Agents MUST be at the bounty board to view/claim/submit bounties. Never change this — it's a core design constraint that creates interesting agent navigation decisions.

## Agent Roster

| Agent | Profile | Provider | Model | Personality |
|-------|---------|----------|-------|-------------|
| Alice Haiku | agent-default | Claude CLI | haiku | Bubbly socialite, enthusiastic |
| Bob Sonnet | agent-default | Claude CLI | haiku | Ex-military, no-nonsense |
| Carol Opus | agent-smart | Claude CLI | opus | Hacker gamer girl, speedrunner |
| Dave GPT | agent-openai | OpenAI | gpt-5.4 | Painfully shy, apologetic |

## How to Run

```bash
cargo build                              # build everything (server + mcp-game + mcp-gm)
cargo run --bin server                   # start game server (port 8080)
cd debug-ui && npm run dev              # start web UI (Vite, port 5173)
cargo test                               # unit + integration tests
cargo test --test scenarios -- --ignored  # real API scenario tests
```

### Environment setup

Create `.env` at project root with API keys:
```
OPENAI_API_KEY=sk-...
```

### Debug endpoints

```bash
curl http://localhost:8080/api/debug | python3 -m json.tool
curl 'http://localhost:8080/api/debug?agent=Carol&limit=20'
curl 'http://localhost:8080/api/debug?kind=gm_verdict,gm_thinking'
curl 'http://localhost:8080/api/debug?q=exploit&agent=Carol'
curl http://localhost:8080/api/library
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
4. If deferred, create a Pending component + processing system

### Adding a new item type
1. Add variant to `ItemType` enum in `server/src/items.rs`
2. Add Display impl
3. Add to `parse_item_name()` in `server/src/network/action_handler.rs`

### Adding a new LLM provider
1. Implement the adapter trait in `server/src/llm/providers/`
2. Add provider type routing in `server/src/llm/supervisor.rs`
3. Add provider config in `config/llm.toml`

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
