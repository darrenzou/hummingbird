# hummingbird_rust

Rust re-implementation of `hummingbirdv2` with the same high-level structure:

- **Config + dotenv** loading (compatible with `hummingbirdv2` `config.json` formats)
- **Two-process harness** (Poly + Kalshi children) communicating via IPC
- **Core logic** for:
  - Kalshi cascade building from Poly book
  - Poly tracked levels and 10%/15% volume-update triggers
  - SQLite persistence of events + orderbook snapshots

## Status

This is a **compiling MVP** that currently uses **synthetic orderbooks** (no real exchange connections yet).
The live Kalshi/Polymarket adapters are intended to be added behind feature flags next.

## Build

```bash
cd /home/ec2-user/hummingbird_rust
cargo build
```

## Run

```bash
./target/debug/hummingbird_rust [config.json]
```

It mirrors `hummingbirdv2` by requiring credentials in `.env` (even though live adapters are not yet implemented).

