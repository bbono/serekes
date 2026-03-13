# Architecture

## Overview

Serekes is a Rust trading bot that trades binary Up/Down prediction markets on Polymarket, using real-time price data from multiple exchanges to inform trading decisions. It follows a **separation of business logic from infrastructure** — strategies contain only trading decisions, while the engine handles all I/O, state management, and order execution.

## Architecture Diagram

```mermaid
graph TD
    subgraph INFRA["INFRASTRUCTURE LAYER"]
        BIN_WS["Binance WS<br/>(aggTrade)"]
        CB_WS["Coinbase WS<br/>(ticker)"]
        DER_WS["Deribit WS<br/>(DVOL index)"]
        CL_WS["Chainlink WS<br/>(oracle)"]
        PM_WS["Polymarket<br/>Price WS"]
        SYNC["Time Sync<br/>(CLOB server)"]
        CHANNELS["tokio::sync::watch channels<br/>Arc&lt;Mutex&lt;VecDeque&gt;&gt; histories<br/>Arc&lt;Mutex&lt;Option&lt;Market&gt;&gt;&gt; shared state"]
    end

    BIN_WS & CB_WS & DER_WS & CL_WS & PM_WS & SYNC --> CHANNELS

    subgraph ENGINE["ENGINE LAYER"]
        SE["StrategyEngine<br/>• execute_tick<br/>• snapshot()<br/>• killswitch"]
        SM["State Machine<br/>Idle ↔ InPosition"]
        OE["Order Execution<br/>• sign+submit<br/>• Limit/Market"]
        TRAIT["Strategy Trait<br/>create_entry_order(ctx, market) → Option&lt;Direction, Order&gt;<br/>create_exit_order(ctx, market, size) → Option&lt;Order&gt;<br/>manages_exit() → bool"]
        SE --> TRAIT
    end

    CHANNELS --> ENGINE

    subgraph BIZ["BUSINESS LOGIC LAYER"]
        BONO["BonoStrategy<br/>• delay entry<br/>• buy higher ask side<br/>• entry-only"]
        FUTB["(Future Strategy B)"]
        FUTC["(Future Strategy C)"]
    end

    TRAIT --> BIZ

    subgraph SUPPORT["SUPPORT SERVICES"]
        TG["Telegram Alerts"]
        MD["Market Discovery<br/>(Gamma API + Binance kline)"]
        CFG["Config (TOML)"]
    end

    subgraph EXT["EXTERNAL SERVICES"]
        E_BIN["Binance<br/>WS + API"]
        E_CB["Coinbase<br/>WS"]
        E_DER["Deribit<br/>WS"]
        E_PM["Polymarket<br/>CLOB+Gamma"]
        E_TG["Telegram<br/>Bot API"]
    end

    E_BIN --> BIN_WS
    E_CB --> CB_WS
    E_DER --> DER_WS
    E_PM --> PM_WS
    E_TG --> TG
```

## Module Structure

```mermaid
graph LR
    SRC["src/"] --> BOT["bot.rs<br/>Entry point: main(), WS spawners, market discovery"]
    SRC --> CFG["config.rs<br/>AppConfig deserialization from TOML"]
    SRC --> TYP["types.rs<br/>Domain types + Strategy trait + EngineState"]
    SRC --> ENG["engine.rs<br/>StrategyEngine: state machine, tick loop, order execution"]
    SRC --> STRAT["strategies/"]
    STRAT --> SMOD["mod.rs<br/>Strategy re-exports"]
    STRAT --> BONO["bono.rs<br/>BonoStrategy implementation"]
```

## Layer Responsibilities

### Infrastructure Layer (`bot.rs`)

- WebSocket lifecycle management (connect, reconnect, backoff)
- Data feed parsing (Binance aggTrade, Coinbase ticker, Deribit DVOL, Chainlink oracle)
- Price history buffering (ring buffers via VecDeque)
- Data broadcast via `tokio::sync::watch` channels
- Market discovery via Polymarket Gamma API
- Binance kline API for strike price
- Time synchronization with Polymarket server
- Graceful shutdown (SIGINT/SIGTERM)

### Engine Layer (`engine.rs`, types in `types.rs`)

- Tick-based execution loop
- State machine (Idle ↔ InPosition)
- TickContext snapshot construction from all feeds
- Killswitch: halts entries when Binance/Coinbase prices diverge beyond threshold
- Order signing and submission via Polymarket SDK (tick size + neg_risk auto-handled by SDK)
- Minimum order size validation before submission
- Order response status handling (Matched/Delayed/Unmatched/Live)
- Limit and Market order support
- Position tracking (token ID, direction, entry price, size)
- Periodic status logging
- Telegram trade alerts

### Business Logic Layer (`strategies/`)

- Pure trading logic — no I/O, no async, no infrastructure
- Receives `TickContext` (read-only market snapshot) and `Market` (Polymarket state)
- Returns `Option<OrderParams>` — the engine handles everything else
- Pluggable via the `Strategy` trait

### Support Services

- **Config** (`config.rs`): TOML deserialization with defaults and validation
- **Telegram** (`telegram.rs`): Non-blocking alert sender via `OnceLock` + `tokio::spawn`
- **Types** (`types.rs`): Shared domain types (`Market`, `TokenSide`, `TokenDirection`, `Strategy` trait, `EngineState`)

## Concurrency Model

The bot uses **Tokio** for async concurrency:

- **4 long-lived WS tasks** — each spawned via `tokio::spawn`, run independently with auto-reconnect
- **1 per-market WS task** — spawned for each active market, aborted on market expiry
- **Main task** — runs the market rotation loop synchronously (discover → tick → cleanup → repeat)
- **Communication** — `watch` channels for latest-value feeds, `Arc<Mutex<>>` for shared mutable state

No thread pool tuning needed — all I/O is async, compute is minimal.

## Data Flow

```mermaid
flowchart LR
    BIN["Binance<br/>aggTrade"] -->|watch::channel| TC["TickContext<br/>(snapshot)"]
    BIN -->|watch::channel| BH["binance_history<br/>(VecDeque)"]
    CB["Coinbase<br/>ticker"] -->|watch::channel| TC
    DER["Deribit<br/>DVOL"] -->|watch::channel| TC
    CL["Chainlink<br/>oracle"] -->|watch::channel| TC
    CL -->|watch::channel| CH["chainlink_history<br/>(VecDeque)"]
    PM["Polymarket<br/>prices"] -->|"Arc&lt;Mutex&lt;Option&lt;Market&gt;&gt;&gt;"| MKT["market.up/down<br/>.best_bid/ask"]

    BH --> TC
    CH --> TC
    MKT --> TC

    TC --> ENTRY["Strategy.create_entry_order()"]
    TC --> EXIT["Strategy.create_exit_order()"]
    ENTRY & EXIT --> OP["OrderParams"]
    OP --> SE["StrategyEngine<br/>sign_and_submit()"]
    SE --> CLOB["Polymarket CLOB"]
```

## Key Design Decisions

1. **Strategy/Engine separation** — Strategies are pure functions of `(TickContext, Market) → Option<Order>`. They never touch I/O, WebSockets, or SDK internals. This makes them trivially testable and interchangeable.

2. **Watch channels for price feeds** — `tokio::sync::watch` provides latest-value semantics, ideal for price feeds where only the most recent value matters. No backpressure concerns.

3. **Dual strike price** — Both Binance kline open price and Chainlink oracle price are fetched for each market. Chainlink is used as the authoritative strike (it's the settlement oracle).

4. **Killswitch on exchange divergence** — If Binance and Coinbase prices diverge beyond a configurable threshold, all new entries are blocked. This protects against feed issues or flash crashes.

5. **Market rotation** — The bot automatically discovers and rotates to new markets as they open, running continuously across market boundaries.

6. **Entry-only vs full-cycle strategies** — The `manages_exit()` flag lets strategies opt out of exit logic. Entry-only strategies (like Bono) place a buy and immediately yield control back to the rotation loop.

## External Dependencies

| Crate | Purpose |
|-------|---------|
| `polymarket-client-sdk` | CLOB client, WS price streams, Gamma API, order signing |
| `tokio` | Async runtime, channels, signals |
| `tokio-tungstenite` | WebSocket connections (Binance, Coinbase, Deribit, Chainlink) |
| `reqwest` | HTTP client (Binance klines, Telegram) |
| `alloy-signer-local` | Polygon wallet signing |
| `k256` | Secp256k1 cryptography |
| `serde` / `toml` | Config deserialization |
| `chrono` | Timestamp handling |
