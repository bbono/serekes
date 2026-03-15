# Architecture

## Overview

Serekes is a Rust trading bot that trades binary Up/Down prediction markets on Polymarket, using real-time price data from multiple exchanges to inform trading decisions. It follows a **separation of business logic from infrastructure** — strategies contain only trading decisions, while the engine handles all I/O, state management, and order execution.

## Architecture Diagram

```mermaid
graph TD
    subgraph INTEGRATIONS["INTEGRATIONS LAYER"]
        BIN_WS["Binance WS<br/>(aggTrade)"]
        CB_WS["Coinbase WS<br/>(ticker)"]
        CL_WS["Chainlink WS<br/>(oracle)"]
        PM_WS["Polymarket<br/>Price WS"]
        SYNC["Time Sync<br/>(CLOB server)"]
        CHANNELS["tokio::sync::watch channels<br/>Arc&lt;Mutex&lt;VecDeque&gt;&gt; histories<br/>Arc&lt;Mutex&lt;Option&lt;Arc&lt;Market&gt;&gt;&gt;&gt; shared state"]
        TG["Telegram Bot<br/>(dedicated thread)"]
        NOTION["Notion<br/>(background worker)"]
        RESOLVER["Polymarket Resolver<br/>(background task)"]
    end

    BIN_WS & CB_WS & CL_WS & PM_WS & SYNC --> CHANNELS

    subgraph ENGINE["ENGINE LAYER"]
        RUN["engine.run()<br/>• market rotation loop<br/>• feed readiness<br/>• tick loop"]
        SE["StrategyEngine<br/>• execute_tick<br/>• snapshot()<br/>• budget tracking<br/>• SDK authentication"]
        OE["Order Execution<br/>• sign+submit<br/>• Limit/Market<br/>• cooldown"]
        STRIKE["Strike Resolution<br/>• exact (Chainlink)<br/>• at-or-before (Binance)"]
        TRAIT["Strategy Trait<br/>create_order(ctx) → Option&lt;Direction, OrderIntent&gt;"]
        RUN --> SE
        SE --> TRAIT
    end

    CHANNELS --> ENGINE

    subgraph BIZ["BUSINESS LOGIC LAYER"]
        BONO["BonoStrategy<br/>• last-30s entry<br/>• buy higher ask (0.85–0.97)<br/>• FOK market order"]
        KONZ["KonzervaStrategy<br/>• placeholder (no-op)"]
    end

    TRAIT --> BIZ

    subgraph SUPPORT["SUPPORT SERVICES"]
        BUDGET["Budget Manager<br/>Arc&lt;Mutex&lt;f64&gt;&gt;"]
        SECRETS["Secret Loader<br/>(load + optional truncate)"]
        CFG["Config (flat TOML)"]
    end

    subgraph EXT["EXTERNAL SERVICES"]
        E_BIN["Binance<br/>WS + API"]
        E_CB["Coinbase<br/>WS"]
        E_PM["Polymarket<br/>CLOB+Gamma"]
        E_TG["Telegram<br/>Bot API"]
        E_NOTION["Notion<br/>API"]
    end

    E_BIN --> BIN_WS
    E_CB --> CB_WS
    E_PM --> PM_WS
    E_PM --> RESOLVER
    E_TG --> TG
    E_NOTION --> NOTION
    E_NOTION --> RESOLVER
```

## Module Structure

```mermaid
graph LR
    SRC["src/"] --> MAIN["main.rs<br/>Entry point: runtime setup,<br/>integration wiring, shutdown"]
    SRC --> COMMON["common/<br/>config.rs, logger.rs, time.rs"]
    SRC --> INT["integrations/<br/>mod.rs (re-exports + shared helpers)"]
    INT --> BIN["binance.rs"]
    INT --> CB["coinbase.rs"]
    INT --> CL["chainlink.rs"]
    INT --> PM["polymarket/<br/>discovery.rs, price_ws.rs, resolver.rs"]
    INT --> TG["telegram.rs<br/>(bot, commands, outbound/inbound)"]
    INT --> NOTION["notion.rs<br/>(save worker, upsert)"]
    SRC --> ENG["engine/<br/>mod.rs (StrategyEngine, snapshot, auth)<br/>run.rs (market loop, tick loop)<br/>order.rs (try_order, sign+submit)"]
    SRC --> STRAT["strategy/<br/>mod.rs, bono.rs, konzerva.rs"]
    SRC --> TYP["types/<br/>market.rs, tick_context.rs, order.rs, summary.rs<br/>Domain types + Strategy trait"]
```

## Layer Responsibilities

### main.rs (startup wiring)

- Loads config and secrets, initializes logger
- Builds tokio runtime, syncs time with Polymarket
- Spawns integrations (price feeds, Telegram, Notion, resolver)
- Creates `StrategyEngine`, authenticates SDK, calls `engine.run()`
- Handles graceful shutdown (SIGINT/SIGTERM)

### Integrations Layer (`integrations/`)

- **Binance** (`binance.rs`): aggTrade WebSocket, price history buffering
- **Coinbase** (`coinbase.rs`): ticker WebSocket
- **Chainlink** (`chainlink.rs`): Polymarket live-data WS, oracle price history
- **Polymarket** (`polymarket/`):
  - `discovery.rs`: market discovery (Gamma API fetch + retry)
  - `price_ws.rs`: per-market bid/ask WebSocket
  - `resolver.rs`: background market resolution checker
- **Telegram** (`telegram.rs`): dedicated OS thread with own tokio runtime, outbound message queue with retry, inbound long-polling with owner-only filtering, command registration and routing
- **Notion** (`notion.rs`): background save worker via mpsc channel, upsert by Name, auto-populates created/updated dates and market URL

Shared helpers in `mod.rs`: exponential backoff, price history push, JSON parsing, generic `lookup_history`.

### Engine Layer (`engine/`)

- `mod.rs`: `StrategyEngine` struct, snapshot construction, SDK authentication, strike price resolution, tick execution
- `run.rs`: market rotation loop (discover → resolve → trade → cleanup), tick loop, feed readiness wait
- `order.rs`: order placement pipeline (budget check → min size → cooldown → sign+submit), fill price resolution

All engine fields are private — the only public API is `new()`, `authenticate()`, and `run()`.

### Business Logic Layer (`strategy/`)

- Pure trading logic — no I/O, no async, no infrastructure
- Receives `TickContext` (read-only market snapshot including trade history)
- Returns `Option<(TokenDirection, OrderIntent)>` — the engine handles everything else
- Pluggable via the `Strategy` trait

### Domain Types (`types/`)

- `Market`: binary market metadata + domain rules (slug construction, time bucketing, PnL computation)
- `TickContext`: read-only snapshot of all feeds passed to strategy each tick
- `TickResult`: outcome of a single tick
- `MarketSummary`: summary of a completed market session (trades, cost, shares)
- `OrderIntent`: strategy's order decision (Limit or Market)
- `TokenDirection`: Up or Down
- `Trade`: executed trade record

### Support Services (`common/`)

- **Config** (`config.rs`): Flat TOML deserialization with defaults and validation
- **Logger** (`logger.rs`): Timestamp + bot name + module path + colored level + message
- **Time** (`time.rs`): Server-synced `now_ms()` with Polymarket offset

## Concurrency Model

The bot uses **Tokio** with a configurable multi-threaded runtime:

- **Runtime** — `bot_worker_threads` configurable (default 2). Tunable for CPU vs throughput tradeoff.
- **3 long-lived WS tasks** — each spawned via `tokio::spawn`, run independently with auto-reconnect
- **1 per-market WS task** — spawned for each active market, aborted on market expiry
- **Main task** — runs `engine.run()` which owns the market rotation loop (discover → tick → cleanup → repeat)
- **Tick loop** — sleeps `1_000_000 / bot_engine_ticks_per_second` microseconds between ticks. Default 1000 ticks/sec (1ms).
- **Telegram thread** — dedicated OS thread with single-threaded tokio runtime, fully isolated from the engine runtime. Runs outbound message queue and inbound command polling concurrently.
- **Notion worker** — tokio task processing save requests from an mpsc channel. Non-blocking from the caller's perspective.
- **Polymarket resolver** — background tokio task that periodically checks completed markets for resolution via the Gamma API, updating Notion records.
- **Communication** — `watch` channels for latest-value feeds, `Arc<Mutex<>>` for shared mutable state (market, budget), `mpsc` for Telegram/Notion message queues

## Data Flow

```mermaid
flowchart LR
    BIN["Binance<br/>aggTrade"] -->|watch::channel| TC["TickContext<br/>(snapshot)"]
    BIN -->|push_history| BH["binance_history<br/>(VecDeque)"]
    CB["Coinbase<br/>ticker"] -->|watch::channel| TC
    CL["Chainlink<br/>oracle"] -->|watch::channel| TC
    CL -->|push_history| CH["chainlink_history<br/>(VecDeque)"]
    PM["Polymarket<br/>prices"] -->|"Arc&lt;Mutex&lt;Option&lt;Arc&lt;Market&gt;&gt;&gt;&gt;"| MKT["market.up/down<br/>.best_bid/ask"]

    BH & CH --> TC
    MKT --> TC

    TC --> ENTRY["Strategy.create_order()"]
    ENTRY --> OI["OrderIntent"]
    OI --> SE["StrategyEngine<br/>budget check → sign_and_submit()"]
    SE --> CLOB["Polymarket CLOB"]
    SE -->|deduct cost| BUDGET["Shared Budget"]
    SE -->|save| NOTION["Notion<br/>(via mpsc)"]
```

## Key Design Decisions

1. **Strategy/Engine separation** — Strategies are pure functions of `TickContext → Option<(Direction, OrderIntent)>`. They never touch I/O, WebSockets, or SDK internals. This makes them trivially testable and interchangeable.

2. **Watch channels for price feeds** — `tokio::sync::watch` provides latest-value semantics, ideal for price feeds where only the most recent value matters. No backpressure concerns.

3. **Dual strike price** — Both Binance history and Chainlink oracle price are looked up from history buffers for each market. The engine decides match semantics per source: Chainlink uses exact timestamp match (it's the settlement oracle); Binance uses latest price at or before market start. The integrations layer provides a generic `lookup_history` function; the business decision lives in the engine.

4. **Budget tracking** — Shared `Arc<Mutex<f64>>` budget is deducted on each buy trade and checked before order placement. When budget drops below $1.00, the engine stops trading. Budget is queryable and settable at runtime via Telegram.

5. **Market rotation** — The engine automatically discovers and rotates to new markets as they open via `engine.run()`, running continuously across market boundaries. Per-market state (trades, cooldowns) is cleared between rotations.

6. **Telegram isolation** — The Telegram bot runs on a dedicated OS thread with its own single-threaded tokio runtime, preventing any Telegram latency or errors from affecting the trading engine. Only messages from the configured `bot_telegram_chat_id` are processed.

7. **Failed order cooldown** — After a CLOB submission failure, the engine waits 3 seconds before attempting another order, preventing rapid-fire failures.

8. **Secret file truncation** — By default (`truncate_secrets_file = true`), the secrets file is emptied after reading at startup, so secrets do not remain on disk. All secrets (polygon key, telegram token, notion secret) are stored in a single `secrets.toml` file.

9. **Flat config** — All configuration lives in a single flat TOML file with no nested sections. Keys are prefixed by their module (`bot_`, `market_`, `engine_`, `logger_`, `feeds_`, `notion_`), making it easy to grep and manage.

10. **Thin main.rs** — `main.rs` is pure startup wiring: load config, spawn integrations, build engine, call `engine.run()`, handle shutdown. All orchestration logic (market rotation, tick loop, feed readiness) lives in the engine.

11. **Non-blocking Notion saves** — Notion API calls are queued via mpsc and processed by a background worker, so the trading engine is never blocked by network latency.

12. **Polymarket resolver** — A background task periodically queries the Gamma API for completed market resolutions, automatically updating Notion records from "completed" to "resolved" with the winning outcome.

13. **Private engine fields** — All `StrategyEngine` fields are private. The public API is three methods: `new()`, `authenticate()`, and `run()`. Internal concerns (snapshot, tick execution, order placement, strike resolution) are fully encapsulated.

## External Dependencies

| Crate | Purpose |
|-------|---------|
| `polymarket-client-sdk` | CLOB client, WS price streams, Gamma API, order signing |
| `tokio` | Async runtime, channels, signals |
| `tokio-tungstenite` | WebSocket connections (Binance, Coinbase, Chainlink) |
| `teloxide` | Telegram Bot API client (long-polling, message sending, command menu) |
| `reqwest` | HTTP client (Telegram via teloxide, Notion API, Gamma API) |
| `alloy-signer-local` | Polygon wallet signing |
| `k256` | Secp256k1 cryptography |
| `serde` / `toml` | Config deserialization |
