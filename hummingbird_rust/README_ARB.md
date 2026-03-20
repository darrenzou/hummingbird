# Multi-level arb (Rust)

## Flow (plan)

1. **Kalshi** connects WS, sends `KalshiBookSnapshot` → **Poly**.
2. **Poly** merges Kalshi book + Poly WS book, runs `strategy::build_cascade`, sends `CascadeOrders`.
3. **Kalshi** places orders, sends `KalshiLevelsDone`.
4. **Poly** tracks per-level Poly liquidity; on 10%/15% moves sends `LevelUpdate` (amend/cancel).
5. Fills → `KalshiFill` → Poly hedges with presigned pool (buy **0.99**, sell **0.01**).

## Database

- **Postgres (RDS):** set `DATABASE_URL` (e.g. `postgresql://user:pass@host:5432/db?sslmode=require`). Schema is created automatically; see `migrations/postgres_init.sql`.
- **MySQL (legacy):** if `DATABASE_URL` is unset, use `DB_HOST`, `DB_USER`, etc.

On every **fill** and **error**, both orderbooks are stored (JSON) in `orderbook_snapshots` linked to `events`.

## Env

- `ARB_TICKER`, `ARB_TOKEN_ID`, balances: `ARB_KALSHI_BALANCE`, `ARB_POLY_BALANCE`, `ARB_SIDE_CAP`
- Polymarket + Kalshi creds as before (see `.env` example in repo)
