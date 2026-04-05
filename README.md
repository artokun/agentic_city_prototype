# Stripe Agentic City

A headless Bevy ECS simulation of a San Francisco city where autonomous Claude AI agents walk around, complete bounties, manage needs (energy/hunger/boredom), work shifts, trade items, and socialize. Each agent is an isolated Claude instance with a unique personality, making all decisions via MCP tool calls.

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│  Claude CLI  │◄───►│  MCP Server  │◄───►│ Game Server  │
│ (per agent)  │     │  (mcp-game)  │     │ (Bevy ECS)   │
│  --sdk-url   │     │  stdio JSON  │     │  Axum HTTP   │
└─────────────┘     └──────────────┘     └──────┬───────┘
                                                │ FlatBuffers
                                                │ WebSocket
                                          ┌─────▼───────┐
                                          │  Web Client  │
                                          │ (TypeScript)  │
                                          └──────────────┘
```

### Components

- **server/** — Bevy ECS headless game server (Rust)
  - 2Hz tick rate (configurable via `TICK_MS`)
  - Axum HTTP + WebSocket on port 8080
  - FlatBuffers binary state broadcast
  - Game Master verification via one-shot Claude agents
- **mcp-game/** — MCP stdio server for agent actions (Rust)
  - Single `game_action` tool with 20+ actions
  - Identity baked in via CLI args (tamper-proof)
- **mcp-gm/** — MCP server for Game Master world queries (Rust)
  - `query_world_state` and `submit_verdict` tools
- **client/** — Web debug monitor (TypeScript + Vite)
  - Agent cards with needs, inventory, thoughts, actions
  - Bounty board, conversations, activity log
  - FlatBuffers deserialization
- **schemas/** — FlatBuffers schema + codegen (Rust + TypeScript)

### Key Features

- **Token-driven energy**: Agent energy = context window remaining. Sleep triggers `/compact`.
- **Game Master AI**: DCC-style System AI verifies bounty completions with sarcastic commentary.
- **Real research**: `search_internet` spawns Claude to do actual web research, produces markdown documents stored on disk.
- **Tile inventory**: Items can exist on tiles (passed-out agents become tile items).
- **Business cards**: Physical contact exchange required before messaging.
- **Scenario tests**: Isolated end-to-end tests with real Claude API.

## Quick Start

```bash
# Build everything
cargo build

# Run the server (default config)
cargo run --bin server

# Run with custom config
TICK_MS=100 CONTEXT_LIMIT=10000 cargo run --bin server

# Run the web client
cd client && npm install && npm run dev
```

## Configuration

All tunable constants are in `server/src/config.rs`, overridable via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `TICK_MS` | 500 | Milliseconds per game tick |
| `CONTEXT_LIMIT` | 150000 | Token budget before energy hits 0 |
| `HUNGER_DECAY` | 0.025 | Hunger decay per tick |
| `BOREDOM_DECAY_IDLE` | 0.05 | Boredom decay when idle |
| `CONTEXT_INTERVAL` | 50 | Ticks between AI context updates |
| `STATUS_INTERVAL` | 200 | Ticks between status messages |
| `HOSPITAL_FEE` | 5 | Gold cost for hospital recovery |
| `DOCUMENTS_DIR` | ./documents | Where research docs are stored |

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
| `/api/gm/document` | POST | Deliver research document |
| `/api/documents` | GET | List all documents |
| `/api/documents/{agent}/{file}` | GET | Read a document |

## Testing

```bash
# Unit + integration tests (no API key needed)
cargo test

# Scenario tests (real Claude API, costs tokens)
cargo test --test scenarios -- --ignored
```
