# Stripe Agentic City

A headless Bevy ECS simulation of a San Francisco city where autonomous AI agents walk around, complete bounties, manage needs (energy/hunger/boredom), work shifts, trade items, and socialize. Each agent is an isolated LLM session with a unique personality, making all decisions via MCP tool calls. Supports multiple LLM providers (Claude CLI, OpenAI Responses API).

## Architecture

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Claude CLI  │◄───►│  MCP Server  │◄───►│ Game Server  │
│ (per agent)  │     │  (mcp-game)  │     │ (Bevy ECS)   │
│  --sdk-url   │     │  stdio JSON  │     │  Axum HTTP   │
└──────────────┘     └──────────────┘     └──────┬───────┘
                                                 │ FlatBuffers
┌──────────────┐     ┌──────────────┐            │ WebSocket
│  OpenAI API  │◄───►│  Built-in    │      ┌─────▼─────────┐
│  (Responses) │     │  Adapter     │      │  Debug UI     │
│  WebSocket   │     │  (in-process)│      │ (TypeScript)  │
└──────────────┘     └──────────────┘      └───────────────┘
```

### Components

- **server/** — Bevy ECS headless game server (Rust)
  - 1Hz tick rate (configurable via `TICK_MS`)
  - Axum HTTP + WebSocket on port 8080
  - FlatBuffers binary state broadcast
  - Unified LLM session engine with provider abstraction
  - Persistent Game Master (System AI) for bounty verification
- **mcp-game/** — MCP stdio server for agent actions (Rust)
  - Single `game_action` tool with 38+ actions
  - Identity baked in via CLI args (tamper-proof)
  - Auto-generates game manual for agent context
- **mcp-gm/** — MCP server for Game Master world queries (Rust)
  - `query_world_state`, `read_document`, `approve`, `reject` tools
- **debug-ui/** — Web debug monitor (TypeScript + Vite)
  - World map with agent positions and building inventory
  - Agent cards with needs bars, inventory, thoughts, recent actions
  - Bounty board, conversations, relationships, activity log
  - FlatBuffers deserialization
- **config/** — LLM provider and profile configuration (TOML)
- **schemas/** — FlatBuffers schema + codegen (Rust + TypeScript)

## Agent Roster

| Agent | Provider | Model | Personality |
|-------|----------|-------|-------------|
| Alice Haiku | Claude CLI | haiku | Bubbly socialite, enthusiastic, uses too many exclamation marks |
| Bob Sonnet | Claude CLI | haiku | Ex-military, no-nonsense, clipped sentences, "copy that" |
| Carol Opus | Claude CLI | opus | Hacker gamer girl, speedrunner, probes for exploits |
| Dave GPT | OpenAI | gpt-5.4 | Painfully shy, apologetic, second-guesses everything |

Agent tools are restricted to MCP only (no Bash, Read, Write, Edit, or file system access). The Game Master retains full tool access for document inspection.

## Key Features

- **Multi-provider LLM engine**: Claude CLI and OpenAI Responses API adapters with TOML-based profiles
- **Token-driven energy**: Agent energy = context window remaining. Sleep triggers `/compact`.
- **Persistent Game Master**: DCC-style System AI verifies bounty completions with sarcastic commentary. Verdicts hit agent session, activity log, and chat log.
- **Exponential spawn backoff**: Failed LLM sessions retry with exponential delays (max 8 retries). GM panics on max failures since the game can't function without it.
- **Physical bounty system**: Bounty tokens are inventory items. Agents must physically go to the board, claim, complete objectives, deposit proof + token, then submit.
- **Real documents**: `create_document` produces real markdown files stored on disk, viewable via REST API, inspectable in inventory.
- **Economy**: Buildings have inventory, services have gold costs, agents earn paychecks from shifts.
- **Environment loading**: `.env` file loaded at startup via `dotenvy` for API keys.

## Quick Start

```bash
# Build everything
cargo build

# Run the server
cargo run --bin server

# Run the debug UI (separate terminal)
cd debug-ui && npm install && npm run dev
# Open http://localhost:5173

# Run with custom config
TICK_MS=100 CONTEXT_LIMIT=10000 cargo run --bin server
```

## Configuration

### LLM Providers (`config/llm.toml`)

```toml
[providers.claude]
type = "claude_cli"
model = "opus"

[providers.openai]
type = "openai_responses"
model = "gpt-5.4"
api_key_env = "OPENAI_API_KEY"

[profiles.agent-default]
provider = "claude"
model = "haiku"
compact_threshold = 50000
tool_sets = ["game"]
```

### Environment Variables

All in `server/src/config.rs`, overridable via env:

| Variable | Default | Description |
|----------|---------|-------------|
| `TICK_MS` | 1000 | Milliseconds per game tick |
| `CONTEXT_LIMIT` | 50000 | Token budget before energy hits 0 |
| `HUNGER_DECAY` | 0.333 | Hunger decay per tick |
| `STATUS_INTERVAL` | 30 | Ticks between AI context updates |
| `DOCUMENTS_DIR` | ./documents | Where research docs are stored |
| `BOUNTIES_FILE` | bounties.json | Initial bounty definitions |

### API Keys

Create a `.env` file at project root:
```
OPENAI_API_KEY=sk-...
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/ws` | GET | WebSocket for FlatBuffers state stream |
| `/api/action` | POST | Submit agent game action |
| `/api/bounties` | POST | Create bounty (Stripe integration) |
| `/api/contracts` | POST | Create multi-step contract |
| `/api/gm/query` | GET | Query world state (JSON) |
| `/api/gm/verdict` | POST | Submit GM bounty verdict |
| `/api/debug` | GET | Activity feed (filterable by agent, kind, text) |
| `/api/library` | GET | Library catalog |
| `/api/documents` | GET | List all agent documents |
| `/api/documents/{agent}/{file}` | GET | Read a specific document |

## Testing

```bash
# Unit + integration tests (no API key needed)
cargo test
# 160 lib + 16 integration + 61 LLM engine tests

# Scenario tests (real Claude API, costs tokens)
cargo test --test scenarios -- --ignored
```

## Debug Endpoints

```bash
# Full debug feed (all events, newest first)
curl http://localhost:8080/api/debug | python3 -m json.tool

# Filter by agent
curl 'http://localhost:8080/api/debug?agent=Carol&limit=20'

# Filter by event type
curl 'http://localhost:8080/api/debug?kind=gm_verdict,gm_thinking'

# Text search
curl 'http://localhost:8080/api/debug?q=exploit&agent=Carol'
```
