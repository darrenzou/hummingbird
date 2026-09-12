# Multi-level arb (Rust)

**Retired.** This describes the historical Rust bot. It is not maintained and
does not work against current Polymarket / Kalshi APIs. See the root README.

## Flow (plan)

1. **Kalshi** connects WS, sends `KalshiBookSnapshot` → **Poly**.
2. **Poly** merges Kalshi book + Poly WS book, runs `strategy::build_cascade`, sends `CascadeOrders`.
3. **Kalshi** places orders, sends `KalshiLevelsDone`.
4. **Poly** tracks per-level Poly liquidity; on 10%/15% moves sends `LevelUpdate` (amend/cancel).
5. Fills → `KalshiFill` → Poly hedges with presigned pool (buy **0.99**, sell **0.01**).

## Database

- **Postgres (RDS):** set `DATABASE_URL` (e.g. `postgresql://user:pass@host:5432/db?sslmode=require`). Schema is created automatically; see `migrations/postgres_init.sql`.
- **MySQL (legacy):** if `DATABASE_URL` is unset, use `DB_HOST`, `DB_USER`, etc.

On every **fill**, **error**, and **abort**, orderbooks are stored in separate tables:
- **kalshi_book**: Kalshi orderbook JSON (bids/asks with cascade_size) per event
- **poly_book**: Polymarket orderbook JSON per event
- **kalshi_orderbook_levels** / **poly_orderbook_levels** (MySQL): per-level rows for each event

## Env

- `ARB_TICKER`, `ARB_TOKEN_ID`, balances: `ARB_KALSHI_BALANCE`, `ARB_POLY_BALANCE`, `ARB_SIDE_CAP`
- Polymarket + Kalshi creds as before (see `.env` example in repo)

## Shutdown (Ctrl+C)

- The parent process ignores SIGINT so the **Poly** and **Kalshi** children can handle it.
- **Kalshi** child: sets a flag, exits its loop, **cancels all resting orders** it placed, notifies Poly with `KalshiAbort` if applicable, and may `record_abort` to the DB.
- **Poly** child: sets a flag, **`record_abort` (`user_interrupt`)** if DB is configured, sends **`Abort { reason: "user_interrupt" }`** to Kalshi so it tears down, then exits.

## Kalshi book: WS vs REST

- The live Kalshi process still uses the **WebSocket** snapshot + deltas.
- If the WS snapshot parses as **empty** (`yes=0 no=0`) but the public REST book has depth (same as `orderbook_web`), the bot **seeds** from **`GET /markets/{ticker}/orderbook?depth=0`** once after connect. Deltas then apply on top.

## Polymarket prices

- CLOB prices are rounded to **3 decimal places** on ingest (WS snapshot, deltas, REST `/book`) so `f64` noise is less likely to make best bid and best ask look identical.

## Poly idle merge (CTF merge)

When the arb bot holds both YES and NO outcome tokens on Polymarket (from hedging), those positions are **idle** until merged. The **poly idle merge** converts matched YES+NO sets back into USDC via the Polymarket Conditional Tokens (CTF) contract’s `mergePositions`.

**When it runs:** After the same 2s post-trade debounce as presign pool refill (`RESIGN_DEBOUNCE_MS`). The Poly process only attempts a merge when there has been no new Kalshi fill in that window, so it doesn’t compete with active hedging.

**Requirements:**
- **ARB_NO_TOKEN_ID** or **polymarket_no_token_id** (in config) must be set to the Polymarket **NO** outcome token ID for the market. The YES token comes from config (`polymarket_token_id`). The bot needs both to:
  1. Look up the market’s `conditionId` from the Gamma API
  2. Query your YES and NO positions via the CLOB API
  3. Call `mergePositions` with `min(yes_position, no_position)` to redeem full sets for USDC
- **POLY_ADDRESS** must equal the address derived from **ETH_PRIV_KEY**. If positions sit in a Polymarket proxy wallet and the signer is different, merge is skipped.
- **POLYGON_RPC_URL** (optional): Polygon RPC for sending the merge transaction. Defaults to a public RPC.

**Config example:**
```json
{
  "pairs": [{
    "polymarket_token_id": "…",   // YES outcome token
    "polymarket_no_token_id": "…", // NO outcome token — enables idle merge
    "kalshi_ticker": "…",
    "neg_risk": true
  }]
}
```

Or set **ARB_NO_TOKEN_ID** in the environment (e.g. in `.env`). Main sets it from `polymarket_no_token_id` if present.

**If disabled:** You’ll see `[poly] idle merge disabled: set ARB_NO_TOKEN_ID (config polymarket_no_token_id) for the NO leg` when the YES token’s `conditionId` is found but the NO token is not configured. Idle merge is then skipped; hedging and cascade behavior are unchanged.

## Accessing Database Data

source .env && mysql -h "$DB_HOST" -P "$DB_PORT" -u "$DB_USER" -p"$DB_PASS" --ssl --ssl-ca="$DB_SSL_CA" "$DB_NAME"
TRUNCATE TABLE events;