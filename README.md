# Hummingbird

**Retired project.** Hummingbird was a Rust cross-exchange arbitrage bot for binary prediction markets on [Polymarket](https://polymarket.com) and [Kalshi](https://kalshi.com). It is published here as a previous project, not as something to run.

Both venues later changed their API processes and trading rules. The integrations in this repo no longer work against current production APIs, and the bot is not maintained.

This is a historical snapshot of the design and the Rust implementation. Do not point it at live credentials or expect it to place orders.

## What it did

Polymarket and Kalshi both list binary YES/NO markets that resolve to the same event. Prices are quoted in cents. A spread exists when Polymarket buyers will pay more for YES than Kalshi’s best bid, and Polymarket sellers ask less than Kalshi’s best ask.

Hummingbird captured that spread by:

1. Resting limit orders on one venue (the **maker**, usually Kalshi).
2. Pre-signing complementary hedge orders on the other venue (the **taker**, usually Polymarket).
3. On a maker fill, immediately posting the pre-signed hedge so YES + NO locked in a small, bounded edge.

Polymarket CLOB orders are EIP-712 signed (secp256k1). Signing after a fill would add latency, so the bot kept a pool of pre-signed GTC orders and refreshed them between fills.

## How it worked

Two child processes, one per venue, talked over length-prefixed IPC pipes. The parent forked both, ignored Ctrl+C, and waited for them to exit so each child could cancel resting orders on shutdown.

```
main
 ├── fork → Polymarket worker   (taker hedge + book + EIP-712 pool)
 └── fork → Kalshi worker       (maker book + cascade orders + fills)
              ▲
              └── Unix pipes, bincode messages (IPC v2)
```

Typical loop:

1. **Kalshi** connected over WebSocket, sent a maker book snapshot to Poly.
2. **Poly** merged that book with its own CLOB book and ran `strategy::build_cascade`.
3. Cascade levels were sized from overlapping depth, cash balances, and a per-side cap. Maker limits had to sit at least **2¢** inside the taker best bid/ask (`EDGE_CENTS`).
4. **Kalshi** placed the cascade, reported live levels, and streamed fills.
5. On a **10% / 15%** drop in taker volume behind a level, Poly sent amend/cancel updates so Kalshi could shrink or pull size without losing queue when possible.
6. A Kalshi fill flipped hedge demand on Poly. The Poly worker submitted matching pre-signed orders (binary slot pool) and re-signed used slots after a short debounce.
7. Offsetting YES+NO Polymarket positions could be merged back to USDC through the Conditional Tokens `mergePositions` path when a NO token id was configured.

Either side aborted if YES traded through **5¢ / 95¢**, on fatal HTTP errors, or on Ctrl+C. HTTP **429** was retried with backoff; other 4xx/5xx were treated as fatal and sent as `AbortFatal` to the peer.

### Process layout

| Module | Role |
|---|---|
| `src/main.rs` | Load config, fork Poly + Kalshi, wait |
| `src/strategy.rs` | Cascade math, edge rule, hedge accumulator |
| `src/maker_runtime.rs` | Shared maker event loop |
| `src/arb_kalshi.rs` / `kalshi_live.rs` | Kalshi WS/REST, RSA-PSS auth, batch place/amend |
| `src/arb_poly.rs` / `poly_live.rs` | Polymarket WS/REST, EIP-712 + HMAC L2 auth |
| `src/poly_merge.rs` | Idle CTF merge of YES+NO back to USDC |
| `src/arb_ipc.rs` | Length-prefixed bincode, 8 MB message cap |
| `src/arb_db.rs` | Optional Postgres or MySQL event + book snapshots |
| `src/error_policy.rs` | 429 retry vs fatal classification |

Maker venue was configurable (`maker: "kalshi"` or `"polymarket"`). The Rust tree also had a Kalshi-as-taker path; the original production shape was Kalshi maker / Polymarket taker.

## Repository layout

```
hummingbird_rust/          Rust crate (the bot)
  src/                     library + `hummingbird_rust` binary
  tests/                   cascade, amend, accumulator, fixture tests
  setup_test/              old connectivity / DB write helpers
  migrations/              Postgres schema reference
SYSTEM_PLAN.md             original language-neutral design notes
```

The earlier C implementation was removed. This archive keeps only the Rust port.

## Status

| | |
|---|---|
| **State** | Retired. Not maintained. |
| **Why** | Polymarket and Kalshi API / regulatory changes made the live path obsolete. |
| **Live trading** | Do not run this against production. Endpoints, auth, and market rules have moved on. |
| **What remains useful** | Process split, IPC protocol, cascade sizing, fill-to-hedge flow, and the test fixtures. |

`SYSTEM_PLAN.md` is the original spec. Some sections still describe the removed C tree or older IPC v1 messages. Prefer the Rust sources when the two disagree.

## Build (historical)

This crate targeted Linux/Unix (`fork`, pipes, `select`). It is left here so the code can be read and compiled for study, not operated.

```bash
cd hummingbird_rust
cargo test
cargo build
```

A `config.json` named markets and balances. Credentials came from a local `.env` (API keys, Ethereum key, Kalshi RSA PEM, optional `DATABASE_URL`). Those files are not in this repo.

## License

This repository is a personal project archive. There is no warranty. Use of exchange APIs is governed by those companies’ current terms; this code does not grant any right to trade on them.
