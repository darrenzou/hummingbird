Multi-Level Limit Order Arbitrage (v2)
======================================

This project is a standalone C implementation of the **Multi-Level Limit Order Arbitrage Redesign**.
It is intentionally written without referencing the original `hummingbird` codebase and follows the
high-level behavior described in the plan:

- Cascading Kalshi limit orders across multiple price levels
- Poly-side volume tracking above/below price thresholds
- Per-level 10%/15% volume change triggers
- Poly hedging via binary presigned order pools (5 copies per power-of-two)
- Simple fixed-size IPC messages for communication between Poly and Kalshi processes
- **SQLite persistence**: on Kalshi fills and on limit-order resizes, both orderbooks (Kalshi + Polymarket) and the action (fill amount/price, or volume before/after and price) are written to a DB

Project layout
--------------

- `Makefile` – build rules for the binaries
- `arb_ipc.h`, `arb_ipc.c` – shared IPC message definitions and pipe send/recv
- `arb_db.h`, `arb_db.c` – SQLite persistence for events and orderbook snapshots
- `arb_poly.h`, `arb_poly.c` – Poly-side order book, volume tracking, presign pool, and hedging logic
- `arb_kalshi.h`, `arb_kalshi.c` – Kalshi-side order book and cascading limit-order logic
- `arb_config.h`, `arb_config.c` – config.json loader (same pair shape as hummingbird; optional globals)
- `main.c` – two-process harness (fork + pipes); loads config and `.env` creds, sets env, then Poly and Kalshi children communicate via IPC
- `kalshi_live.h`, `kalshi_live.c` – Kalshi REST (place/cancel/amend, balance) and WebSocket (orderbook, user_fills)
- `poly_live.h`, `poly_live.c` – Polymarket CLOB REST (EIP-712 signed orders, balance) and WebSocket (market orderbook)

The bot uses **real exchange connections**: Kalshi REST + WebSocket, Polymarket CLOB REST + WebSocket. Credentials are read from `.env` (and optional Kalshi PEM file). No simulation mode.

Config (config.json)
--------------------

Configuration is loaded from a JSON file, similar to hummingbird. By default the bot uses
`config.json` in the current directory; you can pass another path:

```bash
./arb_bot [config.json]
```

**Accepted formats:**

1. **Wrapper (v2):** Root object with a `pairs` array and optional globals:
   ```json
   {
     "db_path": "arb_events.db",
     "side_cap": 1000,
     "kalshi_balance": 2000,
     "poly_balance": 2000,
     "pairs": [
       {
         "polymarket_token_id": "...",
         "kalshi_ticker": "KXLLM1-26MAR31-A",
         "neg_risk": true
       }
     ]
   }
   ```
   Globals are applied to the process (e.g. `db_path` → `ARB_DB_PATH`, balances and `side_cap` → env for the Kalshi child).

2. **Array of pairs (hummingbird-style):** Root is a JSON array of pair objects; no globals (defaults are used).

3. **Single pair object:** Root is one object with `polymarket_token_id`, `kalshi_ticker`, and optional `neg_risk`.

Each pair object must include `polymarket_token_id` and `kalshi_ticker`; `neg_risk` is optional (boolean).
Optional `polymarket_no_token_id`: the NO token for the same market. When set, enables sell-first hedge logic: before placing buy orders, the bot checks if you hold the opposite token and sells that first (e.g. need to buy 15 NO, hold 12 YES → sell 12 YES, buy 3 NO). Required for SIDE_BID (Kalshi buy) hedges.
The first pair is used for live trading; config globals (e.g. `kalshi_balance`, `poly_balance`, `side_cap`) override env defaults when set.

**Dependencies:** Config parsing requires **libcjson** (`-lcjson`, include `<cjson/cJSON.h>`). Install e.g. `libcjson-dev` (Debian/Ubuntu) or `cjson-devel` (Fedora/RHEL).

Building
--------

From the `hummingbirdv2` directory:

```bash
make
```

This produces a single executable:

- `arb_bot` – runs the live arbitrage loop:
- Poly: WebSocket orderbook → send full book to Kalshi; track levels; send volume updates (10%/15%); on Kalshi fills, place CLOB hedge orders (binary presign pool).
- Kalshi: WebSocket orderbook; receive Poly full book; build cascade; place/cancel/amend via REST; subscribe to user_fills; forward fills to Poly. Position rebalance: when bid/ask fills change net YES position, allocate that amount to the opposite side (e.g. buy 18 → add 18 to ask) after 2s debounce.

**Required:** `.env` with Kalshi creds (`KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH` or PEM) and Poly creds (`POLY_ADDRESS`, `POLY_API_KEY`, `POLY_SECRET`, `POLY_PASSPHRASE`, `ETH_PRIV_KEY`). See plan for full flow.

Running
-------

From the `hummingbirdv2` directory:

```bash
./arb_bot [config.json]
```

If Kalshi or Poly credentials are missing, the process exits with an error. Config must include at least one pair (`polymarket_token_id`, `kalshi_ticker`, optional `neg_risk`).

Database (SQLite)
-----------------

Set `ARB_DB_PATH` to a file path (default: `./arb_events.db`). The Kalshi process writes:

- **On startup:** one row in `events` with `action_type='start'` and both Kalshi and Poly orderbook snapshots in `event_orderbook_levels`.

- **On a Kalshi fill:** one row in `events` with `action_type='fill'`, `fill_price_cents`, `fill_amount`, plus full Kalshi and Poly orderbook snapshots in `event_orderbook_levels` (venue `kalshi` / `poly`, side `bid` / `ask`, level_index, price, size).

- **On a limit order resize** (when Poly sends a volume update): one row per updated level in `events` with `action_type='resize'`, `resize_price_cents`, `resize_vol_before`, `resize_vol_after`, `resize_side` (`bid`/`ask`), plus both orderbook snapshots in `event_orderbook_levels`.

Example queries:

```bash
sqlite3 arb_events.db "SELECT id, action_type, fill_price_cents, fill_amount, resize_price_cents, resize_vol_before, resize_vol_after, resize_side FROM events;"
sqlite3 arb_events.db "SELECT event_id, venue, side, level_index, price, size FROM event_orderbook_levels WHERE event_id = 1;"
```

