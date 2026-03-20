# Polymarket–Kalshi Cross-Exchange Arbitrage System
## Complete Technical Specification

> **Purpose:** This document is the canonical description of the arbitrage system.
> It is written in language-neutral terms so it can be used as a blueprint to
> reimplement, extend, or port the system to any programming language or runtime.

---

## Table of Contents

1. [High-Level Concept](#1-high-level-concept)
2. [Arbitrage Logic](#2-arbitrage-logic)
3. [Architecture](#3-architecture)
4. [Configuration and Credentials](#4-configuration-and-credentials)
5. [IPC Message Protocol](#5-ipc-message-protocol)
6. [Polymarket Process (Poly Worker)](#6-polymarket-process-poly-worker)
7. [Kalshi Process (Kalshi Worker)](#7-kalshi-process-kalshi-worker)
8. [Order Sizing Logic](#8-order-sizing-logic)
9. [Fill Handling](#9-fill-handling)
10. [Price-Change Notifications](#10-price-change-notifications)
11. [Abort and Cleanup](#11-abort-and-cleanup)
12. [Database Persistence](#12-database-persistence)
13. [Diagnostic Tool](#13-diagnostic-tool)
14. [API Reference](#14-api-reference)
15. [Cryptography Reference](#15-cryptography-reference)
16. [Known Gaps and Future Work](#16-known-gaps-and-future-work)

---

## 1. High-Level Concept

**Markets:** Both Polymarket and Kalshi run binary prediction markets that
resolve to YES or NO at expiry. Prices on both platforms are in cents (1–99).

**The arb opportunity exists when:**
- `poly_bid_cents > kalshi_best_bid_cents`  AND
- `poly_ask_cents < kalshi_best_ask_cents`

This means Polymarket buyers are willing to pay more for YES than Kalshi's best
bid, and Polymarket sellers are asking less than Kalshi's best ask. The system
captures that spread by:

1. Placing a **resting limit order on Kalshi** at the best bid and best ask.
2. Pre-signing **GTC limit orders on Polymarket** at the complementary prices.
3. When a Kalshi order fills, immediately placing the corresponding pre-signed
   Polymarket order to lock in the profit.

**Why Polymarket orders are pre-signed:** Polymarket uses EIP-712 on-chain
order signing (secp256k1). Signing takes time. By pre-signing a pool of orders
before any fill occurs, the system can post to Polymarket within milliseconds of
a Kalshi fill notification rather than waiting to sign after the fact.

**Price relationship between the two sides:**

```
Kalshi best bid  = K_bid  (integer cents, e.g. 45)
Kalshi best ask  = 100 - best_NO_bid  (e.g. 100 - 56 = 44? → usually K_ask > K_bid)

Polymarket hedge price for bid fills  = 1 - K_bid/100   (e.g. 1 - 0.45 = 0.55)
Polymarket hedge price for ask fills  = 1 - K_ask/100

Rationale: if Kalshi fills our YES-buy at 45c, the complementary NO
position on Polymarket (buy NO = sell YES complement) is priced at 55c.
```

---

## 2. Arbitrage Logic

### 2.1 Arb Check

After both sides have their initial orderbook snapshots, perform one check:

```
arb_ok = (round(poly_bid * 100) > kalshi_best_yes_bid)
       AND (round(poly_ask * 100) < kalshi_best_yes_ask)
```

Where:
- `poly_bid` is in 0–1 scale (e.g. `0.52` = 52 cents)
- `kalshi_best_yes_bid` is integer cents
- `kalshi_best_yes_ask` = `100 - kalshi_best_no_bid`

If the check fails, both sides abort immediately.

### 2.2 Abort Thresholds

If at any point the YES price on either exchange moves outside 5–95 cents,
treat the market as too extreme to arb (effectively resolved) and cancel all
resting orders, then exit.

- Polymarket abort threshold: `best_bid > 0.95` OR `best_ask < 0.05`
- Kalshi abort threshold: `best_yes_bid > 95` OR `best_yes_ask < 5`

---

## 3. Architecture

### 3.1 Process Tree

```
main process
  └── for each market pair in config:
        fork → Pair Manager process
                 ├── fork → Kalshi Worker  (child)
                 └── runs Poly Worker      (parent)
```

Each pair runs independently. The main process only waits for pair managers to
exit. Pair managers wait for their two workers.

### 3.2 Communication

Within each pair, two **unidirectional pipes** connect the two workers:

```
Pipe A:  Poly Worker → Kalshi Worker   (poly sends book data, signing status)
Pipe B:  Kalshi Worker → Poly Worker   (kalshi sends signals, fills, abort)
```

Messages are fixed-size binary structs. Because POSIX guarantees atomic
pipe writes up to `PIPE_BUF` bytes (≥ 4096), no framing is needed as long
as each message fits in that limit (the current messages are all well under
512 bytes).

In a reimplementation, any reliable in-process channel works: Go channels,
Python `multiprocessing.Queue`, Rust `mpsc`, UNIX sockets, etc.

### 3.3 Concurrency Model

Each worker runs a **single-threaded event loop**:
- Service the WebSocket (non-blocking, 10 ms poll timeout)
- Check the IPC pipe (non-blocking)
- Act on any pending state

There is no shared memory between Poly and Kalshi workers beyond the pipes.

---

## 4. Configuration and Credentials

### 4.1 Config File (`config.json`)

```json
[
  {
    "polymarket_token_id": "<uint256 as decimal string>",
    "kalshi_ticker":       "KXHIGHNY-26MAR01-T45",
    "neg_risk":            true
  }
]
```

- **`polymarket_token_id`** — the YES-token ID for the Polymarket CLOB market.
  This is a uint256 stored as a decimal string (not hex).
- **`kalshi_ticker`** — the Kalshi market ticker (e.g. `KXHIGHNY-26MAR01-T45`).
- **`neg_risk`** — boolean. If `true`, the Polymarket market uses the negRisk
  CTF Exchange contract address instead of the standard one.
  (Standard: `0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E`)
  (NegRisk:  `0xC5d563A36AE78145C45a50134d48A1215220f80a`)

The file may be a single JSON object (treated as one pair) for backward
compatibility.

### 4.2 Environment Variables (`.env`)

Loaded by the program from a `.env` file in the working directory.
Multi-line values (RSA PEM blocks) are supported: continuation lines are
any lines after the `KEY=value` line up to the next blank line.

| Variable | Description |
|---|---|
| `POLY_ADDRESS` | `0x…` proxy wallet address (EIP-712 maker) |
| `POLY_API_KEY` | L2 API key UUID |
| `POLY_SECRET` | L2 API secret (base64-encoded HMAC key) |
| `POLY_PASSPHRASE` | L2 API passphrase |
| `ETH_PRIV_KEY` | Ethereum private key (hex, with or without `0x` prefix) |
| `KALSHI_API_KEY_ID` | Kalshi API key UUID |
| `KALSHI_PRIVATE_KEY_PATH` | Path to RSA private key `.pem` file |
| `KALSHI_PRIVATE_KEY_PEM` | Inline RSA PEM (overrides path if set) |
| `DB_HOST` | RDS host (default: `database-1.cz6uu00gwfxx.eu-west-1.rds.amazonaws.com`) |
| `DB_PORT` | RDS port (default: `3306`) |
| `DB_USER` | RDS username (default: `admin`) |
| `DB_PASS` | RDS password |
| `DB_NAME` | Database name (default: `arb`) |
| `DB_SSL_CA` | Path to CA certificate bundle (default: `/certs/global-bundle.pem`) |

### 4.3 Max Pairs

The system supports up to 64 market pairs running in parallel. Each pair
is completely isolated from the others.

---

## 5. IPC Message Protocol

Seven message types flow over the two pipes. All messages are the same fixed
binary size (the union is as wide as the largest member).

### Direction: Poly → Kalshi

| ID | Name | Payload | Trigger |
|----|------|---------|---------|
| 1 | `POLY_BOOK` | `bid, ask` (0–1 float); `bid_vol, ask_vol` (tokens) | After Poly gets initial WS snapshot |
| 3 | `POLY_SIGNING_DONE` | `poly_bid_vol, poly_ask_vol` (filtered tokens); `poly_balance` (USD) | After Poly pre-signs 20 orders |
| 6 | `ABORT` | `reason` (0=no arb, 1=threshold, 2=pipe error) | Price threshold exceeded or fatal error |
| 7 | `POLY_PRICE_UPDATE` | `bid, ask` (0–1 float) | Each time Poly's best bid or ask changes |

### Direction: Kalshi → Poly

| ID | Name | Payload | Trigger |
|----|------|---------|---------|
| 2 | `KALSHI_SIGNAL` | `arb_ok` (0/1); `kalshi_bid, kalshi_ask` (cents) | After arb check |
| 4 | `POLY_REDO_SIGNING` | `kalshi_bid, kalshi_ask` (cents) | When Kalshi book moved since signal |
| 5 | `KALSHI_FILL` | `filled_count` (float); `is_bid` (0/1); `order_id` (string) | On each Kalshi WS fill notification |
| 6 | `ABORT` | `reason` | Fatal error or threshold breach |

### Serialization Note

In the C implementation, messages are raw struct bytes. In other languages,
use any fixed-size binary encoding (e.g. `struct.pack` in Python, `encoding/binary`
in Go). The important property is that each write/read is a single atomic
operation — use a mutex or message queue in languages without OS-level pipe
atomicity guarantees.

---

## 6. Polymarket Process (Poly Worker)

### 6.1 WebSocket Connection

- **Host:** `ws-subscriptions-clob.polymarket.com`
- **Path:** `/ws/market`
- **Port:** 443 (TLS)
- **No authentication required** for market subscriptions.

**Subscribe message** (sent immediately on connection):
```json
{
  "assets_ids": ["<token_id>"],
  "type": "market",
  "custom_feature_enabled": true
}
```

**Keep-alive:** Send text `"PING"` every 25 seconds. Server responds with `"PONG"`.

### 6.2 Orderbook Data Model

Store an in-memory orderbook with sorted price levels:

```
bids: list of (price: float 0–1, size: float tokens), sorted DESC by price
asks: list of (price: float 0–1, size: float tokens), sorted ASC by price
```

**Initial snapshot:** Message with `"bids"` and `"asks"` arrays (each element
is `{"price": "0.52", "size": "100.0"}`). Message `type` is `""` or `"book"`.

**Delta updates:** Message with `type: "price_change"` and a `changes` array.
Each change is `{"side": "BUY"|"SELL", "price": "0.52", "size": "100.0"}`.
- If `size == 0`, remove that price level.
- If price level exists, replace its size.
- Otherwise, insert new level and re-sort.

**Best bid/ask:** `bids[0]` and `asks[0]` after sorting.

### 6.3 State Machine

```
STATE 1: Wait for initial WS snapshot (timeout: 30 s)
  → On snapshot received:
      Send POLY_BOOK to Kalshi (best bid, ask, best bid vol, best ask vol)
      → STATE 2

STATE 2: Wait for KALSHI_SIGNAL (timeout: 60 s)
  → On KALSHI_SIGNAL{arb_ok=0}: abort
  → On KALSHI_SIGNAL{arb_ok=1, kalshi_bid, kalshi_ask}:
      → STATE 3 (REDO_SIGNING label)

STATE 3 [REDO_SIGNING]: Pre-sign orders
  1. Compute filtered_bid_vol = sum of Poly bid levels with price > kalshi_bid/100
     (fall back to best-bid volume if no qualifying levels)
  2. Compute filtered_ask_vol = sum of Poly ask levels with price < kalshi_ask/100
     (fall back to best-ask volume if no qualifying levels)
  3. Pre-sign 10 bid orders + 10 ask orders (see §6.4)
  4. Fetch Poly USDC balance (GET /accounts)
  5. Send POLY_SIGNING_DONE{filtered_bid_vol, filtered_ask_vol, poly_balance}
  6. Snapshot last_sent_bid = best_bid, last_sent_ask = best_ask
  → STATE 4

STATE 4 [MONITORING LOOP]: Run until abort
  Every loop iteration (10 ms tick):
    a. Service WS (receive any deltas)
    b. Send PING if > 25 s since last ping
    c. If best_bid or best_ask changed since last notification:
         Send POLY_PRICE_UPDATE{new_bid, new_ask} to Kalshi
         Update last_sent_bid/ask
    d. If best_bid > 0.95 or best_ask < 0.05:
         Send ABORT{reason=1} to Kalshi; break
    e. Poll pipe (non-blocking):
         POLY_REDO_SIGNING{new_bid, new_ask}:
           Update kalshi_bid/ask; goto STATE 3
         KALSHI_FILL{filled_count, is_bid}:
           Run fill handler (see §9)
         ABORT: goto cleanup
```

### 6.4 EIP-712 Order Pre-Signing

Sign **10 bid orders** and **10 ask orders** = 20 orders total.

**Per-order parameters:**
- `token_id`: the YES token ID (decimal uint256 string)
- `maker`: the proxy wallet address
- `signer`: same as maker
- `taker`: `0x0000000000000000000000000000000000000000`
- `expiration`: `0` (no expiry)
- `nonce`: `0`
- `feeRateBps`: `"0"`
- `signatureType`: `1` (POLY_PROXY)
- `side`: `0` (BUY — Polymarket uses BUY for both directions)
- `salt`: random unique uint64, formatted as decimal string

**Amount calculation:**

For bid orders (placed when Kalshi bid fills — hedge by buying NO/selling YES):
```
bid_price_fraction = 1.0 - (kalshi_bid / 100.0)
per_order_tokens   = filtered_bid_vol / 10          (10 orders divide the volume)
taker_amount_micro = round(per_order_tokens * 1e6)  (tokens in 1e-6 units)
maker_amount_micro = round(bid_price_fraction * per_order_tokens * 1e6)
```

For ask orders (placed when Kalshi ask fills — hedge by buying YES):
```
ask_price_fraction = 1.0 - (kalshi_ask / 100.0)
per_order_tokens   = filtered_ask_vol / 10
taker_amount_micro = round(per_order_tokens * 1e6)
maker_amount_micro = round(ask_price_fraction * per_order_tokens * 1e6)
```

**EIP-712 domain separator:**
```
name:               "CTF Exchange"
version:            "1"
chainId:            137  (Polygon)
verifyingContract:  see neg_risk flag in config
```

**Order struct type hash input:**
```
"Order(uint256 salt,address maker,address signer,address taker,
uint256 tokenId,uint256 makerAmount,uint256 takerAmount,
uint256 expiration,uint256 nonce,uint256 feeRateBps,
uint8 side,uint8 signatureType)"
```

**Signing:**
1. Compute `domainSeparator = keccak256(abi.encode(domainTypeHash, ...fields...))`
2. Compute `structHash      = keccak256(abi.encode(orderTypeHash, ...fields...))`
3. Compute `digest          = keccak256(0x1901 ++ domainSeparator ++ structHash)`
4. Sign `digest` with secp256k1 ECDSA using the ETH private key
5. Recover `v` (27 or 28) by trying both recovery IDs and matching the signer address
6. Signature = `0x` + `r` (32 bytes hex) + `s` (32 bytes hex) + `v` (1 byte hex)

**Order placement body** (POST to `/order`):
```json
{
  "order": {
    "salt":          "<decimal string>",
    "maker":         "0x...",
    "signer":        "0x...",
    "taker":         "0x0000000000000000000000000000000000000000",
    "tokenId":       "<decimal string>",
    "makerAmount":   "<decimal string in micro-units>",
    "takerAmount":   "<decimal string in micro-units>",
    "expiration":    "0",
    "nonce":         "0",
    "feeRateBps":    "0",
    "side":          0,
    "signatureType": 1,
    "signature":     "0x<r><s><v>"
  },
  "owner":     "0x...",
  "orderType": "GTC"
}
```

### 6.5 L2 Authentication (REST)

Polymarket REST calls require HMAC-SHA256 authentication:

```
timestamp  = current Unix seconds (integer string)
message    = timestamp + METHOD + path + body
signature  = base64(HMAC-SHA256(base64_decode(POLY_SECRET), message))
```

Headers:
```
POLY_ADDRESS:    <wallet address>
POLY_SIGNATURE:  <signature>
POLY_TIMESTAMP:  <timestamp>
POLY_API_KEY:    <api key>
POLY_PASSPHRASE: <passphrase>
Content-Type:    application/json
```

### 6.6 Balance Fetch

**GET** `https://clob.polymarket.com/accounts` with L2 auth.

Response may be an array or single object; find the `balance` field.
Value is USDC in whole dollars (float or string).

### 6.7 Order Cancellation

**DELETE** `https://clob.polymarket.com/orders` with L2 auth.

Body:
```json
{"orderIDs": ["<id1>", "<id2>", ...]}
```

Track all placed order IDs (up to 20 = N_SIGNED_ORDERS × 2) for cancellation
on abort.

### 6.8 Portfolio Tracking

Maintain integer counters:
- `yes_held`: YES contracts held (incremented when ask fills → buy YES)
- `no_held`: NO contracts held (incremented when bid fills → buy NO)

After each fill placement:
```
delta = round(orders_placed * per_order_token_vol)
if is_bid_fill: no_held  += delta
else:           yes_held += delta
```

Auto-merge whenever both are positive:
```
to_merge = min(yes_held, no_held)
yes_held -= to_merge
no_held  -= to_merge
# Log: "merged N contract pair(s)"
```

YES+NO pairs resolve to $1.00 each — this is the profit capture.

---

## 7. Kalshi Process (Kalshi Worker)

### 7.1 WebSocket Connection

- **Host:** `api.elections.kalshi.com`
- **Path:** `/trade-api/ws/v2`
- **Port:** 443 (TLS)
- **Authentication:** RSA-PSS headers injected at the HTTP Upgrade handshake
  (before the WS connection is established, not in the subscribe message).

**Auth headers for WS upgrade** (sign the GET request):
```
KALSHI-ACCESS-KEY:       <api_key_id>
KALSHI-ACCESS-TIMESTAMP: <milliseconds since epoch>
KALSHI-ACCESS-SIGNATURE: <base64(RSA-PSS-SHA256(timestamp + "GET" + "/trade-api/ws/v2"))>
```

### 7.2 Orderbook Data Model

Kalshi orderbooks are split into YES and NO bid sides.
Store as two sorted lists of `[price_cents, quantity]` pairs:

```
yes_bids: list of [price (int cents), qty (int contracts)], sorted ASC by price
no_bids:  list of [price (int cents), qty (int contracts)], sorted ASC by price
```

**Best YES bid** = `yes_bids[-1].price` (highest)

**Best YES ask** = `100 - no_bids[-1].price` (100 minus the best NO bid)

Delta updates use `price` (int), `delta` (int, can be negative), and
`side` (`"yes"` or `"no"`). Apply as: `level.qty += delta`; remove if ≤ 0.

### 7.3 Subscribe Messages

**Orderbook subscription** (on connect):
```json
{
  "id": 1,
  "cmd": "subscribe",
  "params": {
    "channels": ["orderbook_delta"],
    "market_ticker": "<ticker>"
  }
}
```

Server sends an `"orderbook_snapshot"` first, then `"orderbook_delta"` events.

**Fill subscription** (after orders are placed, triggered via writable callback):
```json
{
  "id": 2,
  "cmd": "subscribe",
  "params": {
    "channels": ["user_fills"],
    "market_tickers": ["<ticker>"]
  }
}
```

### 7.4 State Machine

```
STATE 1: Connect WS and wait for BOTH:
  - Kalshi orderbook snapshot (from WS)
  - POLY_BOOK message (from IPC pipe)
  (timeout: 60 s)

STATE 2: Arb check (one-shot)
  k_bid = best_yes_bid; k_ask = best_yes_ask
  arb_ok = (round(poly_bid*100) > k_bid) AND (round(poly_ask*100) < k_ask)
  Send KALSHI_SIGNAL{arb_ok, k_bid, k_ask}
  If not arb_ok: exit

STATE 3 [WAIT_SIGNING_DONE]:
  Save sent_bid = k_bid; sent_ask = k_ask; prices_changed = false
  Wait for POLY_SIGNING_DONE (timeout: 120 s)
    - Also service WS; on any orderbook delta that changes best bid/ask:
        set prices_changed = true
  On receipt of POLY_SIGNING_DONE{bid_vol, ask_vol, poly_balance}:
    Drain WS (10 ms) to catch any last deltas
    new_bid = best_yes_bid; new_ask = best_yes_ask
    If prices_changed OR new_bid != sent_bid OR new_ask != sent_ask:
      Send POLY_REDO_SIGNING{new_bid, new_ask}
      Update sent_bid/ask; reset prices_changed
      goto STATE 3
    Else:
      → STATE 4 (place orders)

STATE 4: Place limit orders
  Compute order sizing (see §8)
  Place bid order:  POST /portfolio/orders  (yes, buy,  bid_count, bid_price=k_bid)
  Place ask order:  POST /portfolio/orders  (yes, sell, ask_count, ask_price=k_ask)
  Store bid_order_id, ask_order_id, bid_remaining, ask_remaining
  Subscribe to user_fills
  → STATE 5

STATE 5 [MONITORING LOOP]:
  Every iteration (10 ms tick):
    a. Service WS
    b. Check price thresholds:
         If best_yes_bid > 95 or best_yes_ask < 5:
           Cancel both orders; Send ABORT{reason=1}; exit
    c. If fill_pending:
         fill_pending = false
         Save orderbook snapshot to DB (see §12)
         Send KALSHI_FILL{filled_count, is_bid, order_id} to Poly
         Adjust opposite resting order (see §9.2)
    d. Poll pipe (non-blocking):
         ABORT from Poly: cancel orders; exit
         POLY_PRICE_UPDATE{bid, ask}:
           Check if within 0.01 of Kalshi prices (see §10)
           Possibly place extra orders
```

### 7.5 REST Authentication

All Kalshi REST calls use RSA-PSS-SHA256:

```
timestamp  = milliseconds since epoch (string)
message    = timestamp + METHOD + path_without_query
signature  = base64(RSA-PSS-SHA256(private_key, message, salt_len=digest_size))
```

Headers:
```
KALSHI-ACCESS-KEY:       <api_key_id>
KALSHI-ACCESS-TIMESTAMP: <timestamp>
KALSHI-ACCESS-SIGNATURE: <signature>
Content-Type:            application/json
Accept:                  application/json
```

The path signed is the full API path prefix + endpoint, e.g.:
`/trade-api/v2/portfolio/orders` (not just `/portfolio/orders`).

### 7.6 Place Limit Order

**POST** `https://api.elections.kalshi.com/trade-api/v2/portfolio/orders`

Body:
```json
{
  "ticker":          "<market_ticker>",
  "side":            "yes",
  "action":          "buy" | "sell",
  "count":           <integer contracts>,
  "yes_price":       <integer cents 1–99>,
  "time_in_force":   "good_till_canceled",
  "client_order_id": "<uuid v4>"
}
```

Response contains `order.order_id` — save this to track the resting order.

### 7.7 Cancel Order

**DELETE** `https://api.elections.kalshi.com/trade-api/v2/portfolio/orders/<order_id>`

### 7.8 Amend Order

**POST** `https://api.elections.kalshi.com/trade-api/v2/portfolio/orders/<order_id>/amend`

Body:
```json
{
  "ticker":    "<ticker>",
  "side":      "yes",
  "action":    "buy" | "sell",
  "yes_price": <integer cents>,
  "count":     <new_total_count>
}
```

Used to adjust the opposite resting order's quantity after a partial fill
without cancelling and replacing it (preserving queue position).

### 7.9 Balance Fetch

**GET** `https://api.elections.kalshi.com/trade-api/v2/portfolio/balance`

Response: `{ "balance": { "available_balance": <cents int> } }`

Divide by 100 to get dollars.

---

## 8. Order Sizing Logic

This runs in the Kalshi worker after receiving `POLY_SIGNING_DONE`.

**Inputs:**
- `poly_bid_vol`: filtered Poly bid volume (tokens above Kalshi best bid)
- `poly_ask_vol`: filtered Poly ask volume (tokens below Kalshi best ask)
- `poly_balance`: Poly USDC balance in dollars
- `kalshi_balance`: fetched from Kalshi REST API

**Base sizing (75% of filtered volume):**
```
base_bid   = poly_bid_vol * 0.75
base_ask   = poly_ask_vol * 0.75
base_total = base_bid + base_ask
```

**Budget scaling (when cash exceeds default sizing):**
```
budget = min(kalshi_balance, poly_balance)
if base_total > 0 AND poly_balance > 0 AND kalshi_balance > 0
   AND base_total < budget:
    # We have more cash than the default sizing uses; scale up
    bid_count = floor((base_bid / base_total) * budget)
    ask_count = floor((base_ask / base_total) * budget)
else:
    bid_count = floor(base_bid)
    ask_count = floor(base_ask)

# Enforce minimum 1 contract
bid_count = max(bid_count, 1)
ask_count = max(ask_count, 1)
```

**Order prices:**
```
bid_price = kalshi_best_yes_bid   (join the queue at the best bid)
ask_price = kalshi_best_yes_ask   (join the queue at the best ask)
# Clamp to valid Kalshi range:
bid_price = clamp(bid_price, 1, 99)
ask_price = clamp(ask_price, 1, 99)
```

---

## 9. Fill Handling

### 9.1 Poly Fill Handler (triggered by KALSHI_FILL)

Maintain:
- `g_bid_vol`: per-order volume for bid pool = `filtered_bid_vol / 10`
- `g_ask_vol`: per-order volume for ask pool = `filtered_ask_vol / 10`
- `g_universal_var`: carry-forward for rounding (initially 0.0)

On each fill notification:
```
volume   = g_bid_vol if is_bid else g_ask_vol
effective = filled_count + g_universal_var
if effective <= 0: g_universal_var += filled_count; return

n = ceil(effective / volume)
remainder = effective mod volume

if remainder > volume / 2:
    g_universal_var = +1  # next fill slightly over-counts
else:
    g_universal_var = 0   # or -1 depending on rounding

Place n pre-signed orders from the appropriate pool (bid or ask)
Update portfolio counters
Auto-merge YES+NO if both positive
```

### 9.2 Kalshi Opposite-Side Amend (triggered by fill detection)

When the Kalshi **bid** order partially fills by `fill_amt` contracts:
- `bid_remaining -= fill_amt`
- `new_ask_count = ask_remaining + fill_amt`
- Call `amend_order(ask_order_id, new_ask_count)` to grow the ask order in-place
- `ask_remaining = new_ask_count`

When the Kalshi **ask** order partially fills:
- `ask_remaining -= fill_amt`
- `new_bid_count = bid_remaining + fill_amt`
- Call `amend_order(bid_order_id, new_bid_count)` to grow the bid order
- `bid_remaining = new_bid_count`

**Rationale:** When one side fills, inventory is transferred; the opposite
order is grown to allow further captures from the same spread direction.
`amend` is used instead of cancel+replace to preserve queue position.

---

## 10. Price-Change Notifications

### 10.1 Poly → Kalshi: Price Updates

During the monitoring loop, whenever `best_bid` or `best_ask` changes from
the last value sent, Poly immediately sends `POLY_PRICE_UPDATE{bid, ask}`.

### 10.2 Kalshi: Handling Price Updates

On receipt of `POLY_PRICE_UPDATE`:

```
new_poly_bid_cents = round(bid * 100)
new_poly_ask_cents = round(ask * 100)
cur_k_bid = current kalshi best_yes_bid
cur_k_ask = current kalshi best_yes_ask

# Spread is narrowing — place extra orders to capture it
if abs(bid - cur_k_bid/100) <= 0.01:
    # Poly bid within 1 cent of Kalshi bid: place extra Kalshi bid order
    extra_price = clamp(cur_k_bid, 1, 99)
    place_order(side="yes", action="buy", count=bid_count, price=extra_price)
    track extra order ID for cleanup

if abs(ask - cur_k_ask/100) <= 0.01:
    # Poly ask within 1 cent of Kalshi ask: place extra Kalshi ask order
    extra_price = clamp(cur_k_ask, 1, 99)
    place_order(side="yes", action="sell", count=ask_count, price=extra_price)
    track extra order ID for cleanup
```

Extra order IDs are stored separately from the main two orders and are all
cancelled in the cleanup path.

---

## 11. Abort and Cleanup

### 11.1 Abort Reasons

| Reason | Code | Description |
|--------|------|-------------|
| No arb | 0 | Initial arb check failed |
| Threshold | 1 | Price moved outside 5–95¢ band |
| IPC error | 2 | Pipe EOF or write failure |

### 11.2 Cleanup Sequence

**Poly worker cleanup:**
1. Cancel all placed Polymarket order IDs (`DELETE /orders`)
2. Destroy WebSocket context
3. Exit

**Kalshi worker cleanup:**
1. Cancel main bid order (`DELETE /portfolio/orders/<id>`)
2. Cancel main ask order
3. Cancel all extra orders from price-update triggers
4. Destroy WebSocket context
5. Close database connection
6. Exit

Both workers send an `ABORT` message to the other side before shutting down
their pipe, so the peer can initiate its own cleanup.

---

## 12. Database Persistence

On **every Kalshi fill event**, save the full Kalshi orderbook snapshot to an
Amazon RDS MariaDB instance.

### 12.1 Connection

- Host: RDS endpoint (`DB_HOST` env var)
- Port: 3306 (default)
- TLS: required, using CA bundle at `DB_SSL_CA`
- Database: `arb` (created if not exists)

### 12.2 Schema

```sql
CREATE TABLE IF NOT EXISTS orderbook_snapshots (
  id          BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
  ts_ms       BIGINT      NOT NULL,    -- Unix timestamp in milliseconds
  ticker      VARCHAR(64) NOT NULL,    -- Kalshi market ticker
  fill_order  VARCHAR(64) NOT NULL,    -- Kalshi order_id that triggered snapshot
  fill_amount DOUBLE      NOT NULL,    -- Contracts filled in this notification
  fill_is_bid TINYINT     NOT NULL,    -- 1 = bid order filled, 0 = ask
  yes_levels  TEXT        NOT NULL,    -- JSON: [[price_cents, qty], ...]
  no_levels   TEXT        NOT NULL,    -- JSON: [[price_cents, qty], ...]
  INDEX idx_ticker (ticker),
  INDEX idx_ts    (ts_ms)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
```

### 12.3 Snapshot Format

`yes_levels` and `no_levels` are compact JSON arrays of `[price, qty]` pairs,
sorted ascending by price. Example:
```json
[[40, 120], [41, 300], [43, 85]]
```

### 12.4 Insert

Use a prepared statement (parameterized query) with 7 parameters:
`ts_ms, ticker, fill_order, fill_amount, fill_is_bid, yes_levels, no_levels`

The connection uses `MYSQL_OPT_RECONNECT` (auto-reconnect on drop).

---

## 13. Diagnostic Tool (`market_lookup.py`)

A standalone **read-only** Python script that queries both APIs for all pairs
in `config.json` and prints a summary. Does not place any orders.

### 13.1 What it fetches

**Kalshi (per pair):**
- Market metadata: title, status, yes/no bid/ask (from `GET /markets/<ticker>`)
- Orderbook: best YES bid/ask, quantities (from `GET /markets/<ticker>/orderbook`)
- Volume: total contracts (`volume`), 24-hour contracts (`volume_24h`), open interest

**Polymarket (per pair):**
- Market metadata: question, negRisk, end date, outcome prices (from Gamma API)
- Volume: total USD (`volume`/`volumeNum`), 24-hour USD (`volume24hr`)
- Orderbook: best bid/ask, sizes (from CLOB API `GET /book?token_id=<id>`)

### 13.2 Arb Snapshot

After fetching both sides, computes and prints:
```
spread_bid = poly_bid_cents - kalshi_bid_cents
spread_ask = kalshi_ask_cents - poly_ask_cents
arb_ok = (spread_bid > 0) AND (spread_ask > 0)
```

### 13.3 APIs Used

| Purpose | Method | URL |
|---------|--------|-----|
| Kalshi market info | GET (auth) | `https://api.elections.kalshi.com/trade-api/v2/markets/<ticker>` |
| Kalshi orderbook | GET (no auth) | `https://api.elections.kalshi.com/trade-api/v2/markets/<ticker>/orderbook?depth=0` |
| Poly metadata | GET (no auth) | `https://gamma-api.polymarket.com/markets?clob_token_ids=<token_id>` |
| Poly orderbook | GET (no auth) | `https://clob.polymarket.com/book?token_id=<token_id>` |

---

## 14. API Reference

### 14.1 Polymarket

| Endpoint | Auth | Purpose |
|----------|------|---------|
| `wss://ws-subscriptions-clob.polymarket.com/ws/market` | None | Live orderbook |
| `GET https://clob.polymarket.com/accounts` | L2 HMAC | Balance |
| `POST https://clob.polymarket.com/order` | L2 HMAC | Place order |
| `DELETE https://clob.polymarket.com/orders` | L2 HMAC | Cancel orders (bulk) |
| `GET https://gamma-api.polymarket.com/markets` | None | Market metadata |
| `GET https://clob.polymarket.com/book` | None | Orderbook snapshot |

### 14.2 Kalshi

| Endpoint | Auth | Purpose |
|----------|------|---------|
| `wss://api.elections.kalshi.com/trade-api/ws/v2` | RSA-PSS headers | Live orderbook + fills |
| `GET /trade-api/v2/portfolio/balance` | RSA-PSS | Cash balance |
| `POST /trade-api/v2/portfolio/orders` | RSA-PSS | Place order |
| `DELETE /trade-api/v2/portfolio/orders/<id>` | RSA-PSS | Cancel order |
| `POST /trade-api/v2/portfolio/orders/<id>/amend` | RSA-PSS | Amend order quantity |
| `GET /trade-api/v2/markets/<ticker>` | RSA-PSS | Market metadata |
| `GET /trade-api/v2/markets/<ticker>/orderbook` | None | Orderbook snapshot |

---

## 15. Cryptography Reference

### 15.1 Polymarket: secp256k1 EIP-712

**Hash functions:** Keccak-256 (not SHA-3; different padding byte `0x01`)

**ABI encoding rules:**
- `uint256` / `uint64` / `uint8`: big-endian, zero-padded to 32 bytes
- `address`: zero-padded to 32 bytes (20 bytes right-aligned)
- `string`: keccak256 of UTF-8 bytes (then used as 32-byte word)
- Decimal string `"1234"` → parse as BIGNUM → big-endian 32 bytes

**EIP-712 digest:**
```
0x19 0x01 || domainSeparator (32 bytes) || structHash (32 bytes)
→ keccak256 of the 66-byte prefix
```

**Signature format:** `0x` + `r` (32 bytes, hex) + `s` (32 bytes, hex) + `v` (1 byte, hex)
where `v` ∈ {27, 28}. Recovery ID is found by trying `recid=0` then `recid=1`
and checking which reconstructed public key matches the signer address.

### 15.2 Polymarket: HMAC-SHA256 L2

```
key     = base64_decode(POLY_SECRET)
message = str(unix_seconds) + HTTP_METHOD + path + body
hmac    = HMAC-SHA256(key, message)
sig     = base64_encode(hmac)
```

### 15.3 Kalshi: RSA-PSS-SHA256

```
message   = str(unix_milliseconds) + HTTP_METHOD + path_without_query
# For WS, path = full path including /trade-api/ws/v2
# For REST, path = /trade-api/v2 + endpoint (no query string)

signature = RSA-PSS sign(SHA-256, key, message, salt_length=digest_size(SHA-256)=32)
encoded   = base64_encode(signature)
```

Key format: PKCS#8 RSA private key in PEM format (`-----BEGIN PRIVATE KEY-----`).

---

## 16. Known Gaps and Future Work

### Implemented

- [x] Multi-pair config (one isolated process tree per pair)
- [x] WebSocket orderbook maintenance (snapshot + delta) for both exchanges
- [x] EIP-712 order pre-signing on Polymarket (20 orders pooled)
- [x] RSA-PSS authentication for Kalshi REST and WebSocket
- [x] Arb check with redo-signing loop (price drift detection)
- [x] Budget-scaling order sizing (75% of filtered Poly depth vs available cash)
- [x] Fill-driven Poly order placement with rounding carry
- [x] In-place Kalshi order amendment on partial fills (preserves queue position)
- [x] Portfolio YES/NO tracking and auto-merge on Polymarket
- [x] RDS MariaDB orderbook snapshot on every fill (prepared statements, TLS)
- [x] Price-change notification: Poly → Kalshi via POLY_PRICE_UPDATE
- [x] Extra Kalshi limit orders when Poly price approaches Kalshi spread
- [x] Abort thresholds (5–95¢) on both sides
- [x] Graceful cleanup: cancel all resting orders, close DB, exit

### Gaps / Future Improvements

| Gap | Description |
|-----|-------------|
| **WS reconnect** | Currently exits on WS disconnect. Should reconnect with backoff and re-subscribe, then resync orderbook state. |
| **Partial-fill tracking** | `fill_count` comes from WS `user_fills` which reports the fill amount. Does not yet verify against the REST order status for reconciliation. |
| **Poly price drift after signing** | POLY_PRICE_UPDATE notifies Kalshi of Poly price changes but does not re-trigger the full redo-signing loop. If Poly's price drifts enough to invalidate the arb after orders are placed, the system does not react until the threshold abort. |
| **Slippage on Poly placement** | Pre-signed orders are GTC limit orders. If Poly's market moves adversely between fill and order placement, the limit may not fill. No timeout or retry logic exists. |
| **No position limits** | The system will accumulate unlimited exposure if fills keep arriving. A max net position cap should be enforced. |
| **No PnL tracking** | Profits from YES+NO auto-merge are logged but not recorded in the DB. |
| **Extra order deduplication** | POLY_PRICE_UPDATE may fire on every tick that the price is within 1¢ of Kalshi, potentially placing many extra orders. Rate-limiting or a cooldown should be added. |
| **Graceful SIGTERM** | No signal handler. The process will be killed mid-operation without clean order cancellation if sent SIGTERM. |

---

## Appendix A: File Structure (C Implementation)

```
hummingbird/
├── config.json          # Market pair configuration
├── .env                 # Credentials (not committed)
├── Makefile
├── arb_main.c           # Entry point: process supervisor
├── arb_config.c/h       # Config/credential loading, time helpers
├── arb_ipc.c/h          # IPC message structs and pipe transport
├── arb_poly.c/h         # Polymarket worker (WebSocket, EIP-712, REST)
├── arb_kalshi.c/h       # Kalshi worker (WebSocket, RSA-PSS, REST, DB)
└── market_lookup.py     # Standalone diagnostic tool (Python)
```

## Appendix B: Dependencies (C Implementation)

| Library | Purpose |
|---------|---------|
| `libcurl` | HTTP REST calls |
| `libwebsockets` | WebSocket client |
| `libcjson` / `cjson` | JSON parsing |
| `libssl` / `libcrypto` (OpenSSL) | EIP-712, ECDSA, HMAC, RSA-PSS |
| `libmariadb` | MySQL/MariaDB client for RDS |

Build flags: `-lm` (math), `-lpthread` (required by libwebsockets)

## Appendix C: Porting Notes

When reimplementing in another language, the key behaviours to preserve are:

1. **Atomic IPC:** Each message must be written/read atomically. Use
   language-native channels, queues, or mutex-protected sockets.

2. **Non-blocking WS + pipe polling:** The event loop must be able to service
   both the WebSocket and the IPC channel without either blocking the other.
   In Python: use `asyncio` with `async for` on both. In Go: use goroutines
   with `select`. In Rust: use `tokio::select!`.

3. **EIP-712 must use Keccak-256**, not SHA-3. Most crypto libraries distinguish
   these. In Python: `pysha3` or `eth_hash`. In Go: `golang.org/x/crypto/sha3`
   with `Legacy` option, or `github.com/ethereum/go-ethereum/crypto`.

4. **RSA-PSS salt length** must equal the digest length (32 for SHA-256), not
   auto-detected. Some libraries default to max-length PSS which Kalshi will
   reject.

5. **EIP-712 address encoding:** zero-pad to 32 bytes. A 20-byte Ethereum
   address becomes a 32-byte word with 12 leading zero bytes.

6. **POLY_PRICE_UPDATE rate limiting:** Add a minimum interval (e.g., 100 ms)
   between successive POLY_PRICE_UPDATE sends to avoid flooding the Kalshi
   worker with extra orders when the market is ticking rapidly.
