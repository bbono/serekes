# Workflow

## Startup (bot.rs)

1. Load `config.toml` — validate asset (btc/eth/sol/xrp) and interval (5/15)
2. Spawn 4 WebSocket feed tasks (all concurrent):
   - **Binance** — `aggTrade` stream → `binance_tx` (primary price oracle)
   - **Coinbase** — `ticker` stream → `coinbase_tx` (secondary oracle)
   - **Deribit** — DVOL index → `dvol_tx` (implied volatility)
   - **Chainlink** — Polymarket live-data WS → `chainlink_tx` + `chainlink_history` (settlement oracle)
3. Sync server time offset with Polymarket CLOB
4. Init Telegram, build `StrategyEngine` with chosen strategy (e.g. `BonoStrategy`)
5. Authenticate Polymarket SDK (if trading enabled)
6. Wait for all feeds (Binance, Coinbase, Chainlink, DVOL) before starting engine

## Market Rotation Loop

```
① DISCOVER MARKET
│  Compute time bucket, check slugs via Gamma API
│  Extract Up/Down token IDs, fetch strike price from Binance kline
│
② LOOKUP CHAINLINK STRIKE PRICE
│  Find exact match (ts == started_ms) in chainlink_history
│  No match → skip market, wait for expiry, rotate
│
③ UPDATE SHARED MARKET
│  Set strike_price (chainlink), initial orderbook mid prices
│
④ SPAWN POLYMARKET WS TASKS
│  - Orderbook stream (up + down tokens)
│  - Price stream (last_trade_price, best_bid, best_ask)
│
⑤ WAIT FOR ORDERBOOK
│  Poll until both up.best_bid > 0 and down.best_bid > 0
│
⑥ TICK LOOP (1ms) — until market expires
│  → execute_tick() each iteration
│
⑦ CLEANUP
│  Abort orderbook + price WS tasks
│  Clear traded_slugs
│  Rotate to next market
```

## Tick Loop (`execute_tick` — every 1ms)

```
① KILLSWITCH
│  |binance - coinbase| > exchange_divergence_threshold (absolute $)?
│  Both feeds connected?
│  YES → log periodically, RETURN (full freeze — no entries, no exits)
│
② IN-POSITION CHECKS (if state == InPosition)
│  Token belongs to current market?
│  YES → strategy.check_exit(ctx, market, position_size) → Some(OrderParams)?
│        YES → execute_sell(token_id, order) → RETURN
│
③ ENTRY MATRIX (if state == Idle)
│  Already traded this slug? → skip
│  strategy.check_entry(ctx, market) → Some((direction, OrderParams))?
│     YES → execute_buy(token_id, direction, slug, order)
│
④ LOG (every log_interval_secs)
│  [ENGINE] State | entry price | position size
```

## Strategy Trait

Strategies implement two methods:

```rust
trait Strategy {
    // Called each tick when idle. Return Some((direction, order)) to buy.
    fn check_entry(&self, ctx: &TickContext, market: &Market) -> Option<(TokenDirection, OrderParams)>;

    // Called each tick while holding a position.
    // Return Some(OrderParams) to sell, None to keep holding.
    fn check_exit(&self, ctx: &TickContext, market: &Market, position_size: f64) -> Option<OrderParams>;
}
```

**TickContext** provides: `binance_price`, `binance_ts`, `coinbase_price`, `coinbase_ts`, `chainlink_price`, `chainlink_ts`, `dvol`, `now_ms` (time-offset adjusted).

**OrderParams**: `Limit { price, size }` or `Market { amount }`.

The engine handles all infrastructure: state machine, order signing,
killswitch, logging, and Telegram alerts. The strategy only decides
*when* to trade and *what order* to place.

## State Machine

```
Idle ──[check_entry returns order]──→ InPosition
 ↑                                       │
 └───[check_exit returns order]──────────┘
```

- **Idle** — No position. Engine calls `check_entry` on the current market.
- **InPosition** — Holding shares. Engine calls `check_exit` each tick.

## Order Execution

### execute_buy (engine)

- **Live mode**: Signs order via Polymarket SDK → submits to CLOB. On success → InPosition. On failure → stays Idle.
- **Sim mode**: Immediately sets InPosition with the order's price/size. No on-chain interaction.

### execute_sell (engine)

- **Live mode**: Signs order → submits. On success → Idle. On failure → stays InPosition.
- **Sim mode**: Calculates PnL from entry price, resets to Idle.

### sign_and_submit (engine)

Handles both Limit and Market order types:
- Converts price/size to `Decimal` for the Polymarket SDK
- Builds the order via SDK builder pattern
- Signs with wallet private key
- Posts to Polymarket CLOB
- Returns success/failure

## Market Discovery

On startup (and after each market expires):

1. Computes current time bucket: `(now / interval_secs) * interval_secs`
2. Checks two slugs: current bucket and next bucket (e.g. `btc-updown-5m-1710000000`)
3. Fetches market metadata from Polymarket Gamma API
4. Extracts Up/Down token IDs from market outcomes
5. Fetches strike price from Binance kline API (candle open price)
6. Looks up chainlink strike price from history (exact timestamp match with started_ms)
7. Creates `Market` struct in `shared_market` with strike price and initial prices
8. Subscribes to orderbook + price WebSocket streams for both tokens

## Data Feeds

| Feed | Source | Channel | Update Rate |
|------|--------|---------|-------------|
| Binance price | `aggTrade` WS | `watch<(f64, i64)>` | ~100ms |
| Coinbase price | `ticker` WS | `watch<(f64, i64)>` | ~1s |
| Chainlink price | Polymarket live-data WS | `watch<(f64, i64)>` + history deque | on update |
| Deribit DVOL | `deribit_volatility_index` WS | `watch<f64>` | ~5s |
| Polymarket orderbook | SDK `subscribe_orderbook` | `Arc<Mutex<Option<Market>>>` | on update |
| Polymarket prices | SDK `subscribe_prices` | `Arc<Mutex<Option<Market>>>` | on update |

All feeds auto-reconnect on disconnect with exponential backoff (5s–60s).

## Config Structure

```toml
[wallet]          # Private key, trading_enabled, RPC URL
[market]          # Asset (btc/eth/sol/xrp), interval_minutes (5/15)
[engine]          # budget, exchange_divergence_threshold, log_interval_secs
[strategy.bono]   # Strategy-specific config
[telegram]        # bot_token, chat_id
```
