//! Hummingbird — isolated venue workers, bounded IPC, fill-path without signing.
//!
//! Retired archive. Read the repository README first (design notes for a
//! low-latency backend). Live APIs have moved on.
//!
//! # Process model
//!
//! `main` forks two Unix processes. They share **only** pipes ([`types::ArbMsg`]):
//!
//! 1. **Maker** — rest / amend / cancel a cascade; stream fills.
//! 2. **Taker** — size the cascade from the other book; hedge with a pre-signed pool.
//!
//! Default production shape: **Kalshi maker / Polymarket taker**.
//! `ARB_MAKER` / config `maker` can flip it.
//!
//! Each worker is a single-threaded loop (WS + `select` on the pipe). Crypto and
//! cascade math run *before* a fill, or *after* a debounce — not in the hedge POST.
//!
//! | Start here | Role |
//! |---|---|
//! | [`strategy`] | Cascade math and the 2¢ edge rule |
//! | [`types`] | IPC messages |
//! | [`maker_runtime`] | Shared maker event loop |
//! | [`arb_kalshi`] / [`kalshi_live`] | Kalshi process + WS/REST |
//! | [`arb_poly`] / [`poly_live`] | Polymarket process + WS/REST + hedge pool |
//! | [`kalshi_taker`] | Kalshi hedges, Polymarket makes |
//! | [`poly_merge`] | Idle YES+NO → USDC |
//! | [`arb_ipc`] | Length-prefixed bincode, 8 MiB cap |
//! | [`arb_config`] | `config.json` + `.env` |
//! | [`arb_db`] | Optional DB snapshots (not on the fill path) |
//! | [`error_policy`] | 429 retry vs fatal abort |
//! | [`shutdown`] | Ctrl+C flag in the children |

pub mod arb_config;
pub mod arb_db;
pub mod arb_ipc;
pub mod arb_kalshi;
pub mod arb_poly;
pub mod error_policy;
pub mod kalshi_live;
pub mod kalshi_taker;
pub mod maker_runtime;
pub mod poly_live;
pub mod poly_merge;
pub mod shutdown;
pub mod strategy;
pub mod taker_runtime;
pub mod types;
