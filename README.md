# Hummingbird

Rust system for **cross-venue prediction-market arbitrage**: rest a cascade of limits on one exchange, hedge fills on the other, with signing and sizing kept off the fill path.

**Retired.** Polymarket and Kalshi later changed APIs and rules. This repo is a design archive, not a live trader. Do not run it against production.

It is meant to be read as a **low-latency backend**: isolated processes, a bounded IPC protocol, a single-threaded event loop per venue, and work that cannot happen in a fill window (EIP-712 signing, cascade math) done *before* the fill arrives.

## Problem

Both venues list the same binary YES/NO event. Prices are in cents. A spread exists when Polymarket’s bid is above Kalshi’s bid and Polymarket’s ask is below Kalshi’s ask.

The latency problem is not “compute the spread.” It is: **a maker fill arrives over WebSocket; the hedge must already be signed and sized**, or the other book moves and the edge is gone. Polymarket CLOB orders are EIP-712 (secp256k1). Signing after the fill is too late.

## Design

```
                    ┌─ pipe: books, fills, abort ─┐
                    │     length-prefixed bincode │
parent              ▼                             ▼
 (fork, wait)    Kalshi worker                 Polymarket worker
 SIGINT ignored  maker event loop              taker event loop
 until children  WS + IPC poll                 WS + IPC poll
 exit            rest / amend / cancel         cascade + hedge pool
                 RSA-PSS REST                  EIP-712 pre-sign + HMAC L2
```

Usual production shape: **Kalshi maker / Polymarket taker** (`maker` in config flips it).

| Constraint | Choice | Why |
|---|---|---|
| Two venue APIs, two failure domains | `fork` — one process per venue, no shared heap | A WS stall or REST hang on one side cannot lock the other. Cleanup (cancel resting orders) stays local. |
| Fill → hedge must not wait on crypto | Pre-signed GTC pool, binary slot sizes (`1, 2, 4, …`) | Cover a fill with `O(log n)` already-signed orders; re-sign used slots *after* a 2s debounce, not in the fill handler. |
| Coordination without a shared lock | Unidirectional Unix pipes, `select` + length-prefixed bincode, 8 MiB cap | No mutex across venues. A bad peer cannot grow the receiver without bound. |
| Hot path must be serial and cheap | One thread per worker: service WS, poll IPC, act | No lock convoys on book or slot state. The loop is the concurrency model. |
| Do not rest size you cannot hedge | Cascade: 2¢ inside taker BBO, 75% of remaining depth, `side_cap` | Size is computed from *current* taker liquidity, not a static qty. |
| Depth disappears under you | 10% / 15% taker-volume drop → amend or cancel | Shrink or pull before the hedge book is gone; prefer amend to keep queue. |
| Exchange says stop | 429 → exponential backoff (max 8). Other 4xx/5xx → `AbortFatal` to peer | Retry only what is transient. Fatal errors tear both sides down together. |
| Shutdown mid-book | Parent ignores SIGINT; children cancel, notify, then exit | Resting orders are not left on the venue because the supervisor died first. |

Parent does not trade. It loads config, applies pair 0 to the environment (children are separate address spaces after `fork`), opens two pipes, forks, and `waitpid`s.

## Hot path

When a Kalshi fill hits the maker WS:

1. Maker sends `MakerFill` on the pipe (order id, side, cents, size).
2. Taker decomposes `filled_count` into pre-signed slots and **POSTs them immediately**.
3. Hedge demand is accumulated; a throttle fires a batch once `|pending|` exceeds a contract threshold so tiny fills do not spam the CLOB.
4. Used slots are re-signed only after `RESIGN_DEBOUNCE_MS` (2s) with no new fill — idle work, not fill work.
5. Optional: matched YES+NO on Polymarket is merged back to USDC on the same idle window (`poly_merge`).

Cascade *placement* is also off the fill path: taker runs `strategy::build_cascade` when books update, maker batch-places, then both sit in the WS/IPC loop.

Prices abort at 5¢ / 95¢ (market effectively resolved). Either worker can send `AbortFatal`; the peer cancels and exits.

## Protocol

`types::ArbMsg` (IPC v2). Typical Kalshi-maker / Poly-taker sequence:

1. `MakerBookSnapshot` (+ later deltas)
2. `CascadeOrders` — sized rungs + taker book
3. `MakerLevelsDone` — resting ids / sizes
4. `TakerLevelVolUpdate` / `LevelUpdate` — amend or cancel
5. `MakerFill` → hedge
6. `Abort` / `AbortFatal`

Framing: `u32` LE length + bincode. Reject `len > 8 MiB` before allocating the body.

## Code map

Read `hummingbird_rust/src/lib.rs` first. Every file has a header.

| File | Role |
|---|---|
| `main.rs` | Supervisor: config, `fork`, wait |
| `strategy.rs` | Cascade math, 2¢ edge, hedge accumulator |
| `types.rs` | IPC messages |
| `maker_runtime.rs` | Shared maker loop (venue via traits) |
| `arb_kalshi.rs` / `kalshi_live.rs` | Kalshi process, RSA-PSS, WS/REST |
| `arb_poly.rs` / `poly_live.rs` | Poly process, EIP-712 pool, HMAC L2, WS/REST |
| `kalshi_taker.rs` | Flipped mode: Kalshi hedges, Poly makes |
| `poly_merge.rs` | Idle CTF `mergePositions` |
| `arb_ipc.rs` | Pipes, length prefix, `select` |
| `arb_db.rs` | Optional Postgres / MySQL snapshots (off hot path) |
| `error_policy.rs` | 429 vs fatal |
| `tests/` | Cascade, edge, amend, accumulator, JSON fixtures |

`SYSTEM_PLAN.md` is the original language-neutral spec. Prefer the Rust sources if they disagree (IPC is v2 here).

## What this is not

Not a kernel-bypass / FPGA / colocated matching-engine stack. Venue RTT dominates. The design is about **not adding process, lock, or crypto latency on top of that RTT**, and about failing closed when a book or API is no longer trustworthy.

## Build (study only)

Linux/Unix (`fork`, pipes, `select`). For reading and tests, not operation.

```bash
cd hummingbird_rust
cargo test
```

Credentials were local `.env` (not in this repo). `config.json` is a placeholder.

## License

Personal archive. No warranty. Exchange terms govern any use of their APIs.
