# Workflow

## Startup (main.rs)

1. Load `config.toml` — validate asset (btc/eth/sol/xrp) and interval (5/15)
2. Acquire singleton lock (`.bot.lock`) — prevents duplicate instances
3. Spawn 5 WebSocket feed tasks (all concurrent):
   - **Binance** — `aggTrade` stream → `binance_tx` (primary price oracle)
   - **Coinbase** — `ticker` stream → `coinbase_tx` (secondary oracle)
   - **Deribit** — DVOL index → `dvol_tx` (implied volatility)
   - **Polymarket Orderbook** — SDK orderbook stream → `shared_markets` (Arc<Mutex<HashMap>>)
   - **Polymarket Prices** — SDK price stream → `shared_markets` (last_trade_price, best_bid, best_ask)
4. Sync server time offset with Polymarket CLOB
5. Fetch on-chain USDC balance (if trading enabled)
6. Init Telegram, build `StrategyEngine` with chosen strategy (e.g. `BonoStrategy`)
7. Authenticate Polymarket SDK (if trading enabled)
8. Wait for Binance first price before starting engine
9. Enter 1ms tick loop

## Tick Loop (`execute_tick` — every 1ms)

```
① KILLSWITCH
│  |binance - coinbase| > exchange_divergence_threshold (absolute $)?
│  Both feeds connected?
│  YES → log every 5s, RETURN (full freeze — no entries, no exits)
│
② PENDING STATE CHECK
│  State = PendingBuy or PendingSell?
│  YES → reconcile_with_chain() every 2s or after 5s timeout → RETURN
│
③ IN-POSITION CHECKS (if state == InPosition)
│  ├─ Market expired (time_to_expiry <= 0)?
│  │  YES → settle binary payout (Up/Down) → reset to Idle → RETURN
│  ├─ strategy.check_exit(ctx, market, position_size) → Some(OrderParams)?
│  │  YES → execute_sell(market, order) → RETURN
│  └─ Periodic reconcile_with_chain() every 15s
│
④ UPDATE ATR
│  ATR monitor tracks Binance price volatility
│
⑤ ENTRY MATRIX (if state == Idle)
│  For each market:
│     ├─ Already traded this session? → skip
│     └─ strategy.check_entry(ctx, market, balance) → Some(OrderParams)?
│        YES → execute_buy(market, order) → break
│
⑥ LOG (every log_interval_secs)
│  [ENGINE] State | Balance | Asset Price | DVOL | Market Price | Spread
│
⑦ MARKET ID TRACKING
   If no active market, adopt newly discovered market ID
```

## Strategy Trait

Strategies implement two methods:

```rust
trait Strategy {
    // Called each tick for idle markets. Return Some(OrderParams) to buy.
    fn check_entry(&self, ctx: &TickContext, market: &Market, balance: f64) -> Option<OrderParams>;

    // Called each tick while holding a position.
    // Return Some(OrderParams) to sell, None to keep holding.
    fn check_exit(&self, ctx: &TickContext, market: &Market, position_size: f64) -> Option<OrderParams>;
}
```

**TickContext** provides: `binance_price`, `coinbase_price`, `dvol`, `now_ms` (time-offset adjusted).

**OrderParams**: `Limit { price, size }` or `Market { amount }`.

The engine handles all infrastructure: state machine, order signing, on-chain reconciliation,
killswitch, expiry settlement, logging, and Telegram alerts. The strategy only decides
*when* to trade and *what order* to place.

## State Machine

```
Idle ──[check_entry returns order]──→ PendingBuy ──[on-chain confirmed]──→ InPosition
 ↑                                       │ (timeout 5s → Idle)                │
 │                                       │                                    │
 └───────────────────────────────────────┴──[check_exit returns order]──→ PendingSell
                                                                          │ (timeout 5s → InPosition)
                                                                          │ (confirmed → Idle)
```

- **Idle** — No position. Engine iterates markets, calls `check_entry` on each.
- **PendingBuy** — Buy order submitted to Polymarket CLOB. Engine polls chain via `reconcile_with_chain()` every 2s. If shares appear on-chain → InPosition. If 5s timeout → Idle.
- **InPosition** — Holding shares. Engine checks expiry settlement, then calls `check_exit`. Reconciles position size with chain every 15s.
- **PendingSell** — Sell order submitted. If shares disappear on-chain → Idle. If 5s timeout → InPosition.

## Order Execution

### execute_buy (engine)

- **Live mode**: Sets PendingBuy → signs order via Polymarket SDK → submits to CLOB. On failure, reverts to Idle.
- **Sim mode**: Immediately sets InPosition with the order's price/size. No on-chain interaction.

### execute_sell (engine)

- **Live mode**: Sets PendingSell → signs order → submits. On failure, reverts to InPosition.
- **Sim mode**: Calculates PnL from entry price, updates simulated balance, resets to Idle.

### sign_and_submit (engine)

Handles both Limit and Market order types:
- Converts price/size to `Decimal` for the Polymarket SDK
- Builds the order via SDK builder pattern
- Signs with wallet private key
- Posts to Polymarket CLOB
- Returns success/failure

## On-Chain Reconciliation

`reconcile_with_chain()` verifies engine state against Polygon (500ms timeout):

1. **USDC balance** — `balanceOf` on USDC contract (`0x2791B...`), syncs `simulated_balance` if drift > $0.05
2. **CTF position** — `balanceOf(address, tokenId)` on CTF contract (`0x4D97d...`), syncs `position_size`
3. **Pending confirmation**:
   - PendingBuy: shares > 0.01 on-chain → InPosition. Timeout 5s → Idle.
   - PendingSell: shares < 0.01 on-chain → Idle. Timeout 5s → InPosition.
4. **Drift detection**: If InPosition and chain shares differ from engine state → sync.

Uses raw JSON-RPC `eth_call` (no ethers dependency). Token IDs are converted from decimal to 256-bit hex via `decimal_to_hex256`.

## Market Discovery

On startup (and after each market expires), the Polymarket orderbook task:

1. Computes current time bucket: `(now / interval_secs) * interval_secs`
2. Checks two slugs: current bucket and next bucket (e.g. `btc-updown-5m-1710000000`)
3. Fetches market metadata from Polymarket Gamma API
4. Extracts Up/Down token IDs from market outcomes
5. Fetches strike price from Binance kline API (candle open price)
6. Creates `Market` structs in `shared_markets` with initial prices
7. Subscribes to orderbook + price WebSocket streams for both tokens

## Data Feeds

| Feed | Source | Channel | Update Rate |
|------|--------|---------|-------------|
| Binance price | `aggTrade` WS | `watch<f64>` | ~100ms |
| Coinbase price | `ticker` WS | `watch<f64>` | ~1s |
| Deribit DVOL | `deribit_volatility_index` WS | `watch<f64>` | ~5s |
| Polymarket orderbook | SDK `subscribe_orderbook` | `Arc<Mutex<HashMap>>` | on update |
| Polymarket prices | SDK `subscribe_prices` | `Arc<Mutex<HashMap>>` | on update |

All feeds auto-reconnect on disconnect with 5s backoff.

## Config Structure

```toml
[wallet]          # Private key, trading_enabled, RPC URL
[market]          # Asset (btc/eth/sol/xrp), interval_minutes (5/15)
[engine]          # exchange_divergence_threshold, log_interval_secs
[strategy.bono]   # Strategy-specific config
[telegram]        # bot_token, chat_id
```
