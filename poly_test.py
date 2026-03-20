"""
poly_test.py

Python rewrite of poly_test.c.

Polymarket NYC weather market benchmark:
  1. REST  GET  gamma-api/markets?keyword=New+York+temperature  → token_id
  2. WSS   /ws/market subscribe with token_id  → receive "book" snapshot (timed)
  3. REST  POST clob/order   → place EIP-712 signed limit order  (timed)
  4. REST  DELETE clob/orders/{id}             → cancel order     (timed)
  5. Print timing summary

Credentials loaded from .env:
  POLY_ADDRESS     – Polymarket proxy wallet address (0x...) — funder
  ETH_PRIV_KEY     – hex private key of signing wallet (with or without 0x)
  L2 API creds are derived from the private key via create_or_derive_api_creds()

Usage:
  source .env && python3 poly_test.py
"""

import asyncio
import json
import os
import pathlib
import sys
import time
from typing import Any, Dict, List, Optional, Tuple

import requests
import websockets
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import OrderArgs, OrderType, PartialCreateOrderOptions
from py_clob_client.order_builder.constants import BUY

# ── Config ────────────────────────────────────────────────────────────────── #

GAMMA_BASE = "https://gamma-api.polymarket.com"
CLOB_BASE  = "https://clob.polymarket.com"
WS_URI     = "wss://ws-subscriptions-clob.polymarket.com/ws/market"

# Standard CTF Exchange (non-negRisk markets)
CTF_EXCHANGE     = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
# NegRisk CTF Exchange (used by daily temperature / negRisk markets)
CTF_EXCHANGE_NEG = "0xC5d563A36AE78145C45a50134d48A1215220f80a"

CHAIN_ID = 137   # Polygon mainnet
SIG_TYPE = 1     # POLY_PROXY

# Slug pattern for NYC daily high-temperature events on Polymarket.
# Month names Polymarket uses in slugs (lowercase, full name):
_MONTH_NAMES = ["january","february","march","april","may","june",
                "july","august","september","october","november","december"]

# 1 token @ $0.01 — won't fill, safely rests then gets cancelled
MAKER_AMOUNT = 10000    # 0.01 USDC  (6 decimals)
TAKER_AMOUNT = 1000000  # 1 token    (6 decimals)
ORDER_SIDE   = 0        # BUY

WS_TIMEOUT_S   = 15     # max seconds to wait for WS snapshot
DELTA_CAP      = 5      # price_change messages to collect before closing WS


# ── .env loader ───────────────────────────────────────────────────────────── #

def _load_dotenv(path: pathlib.Path) -> None:
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # strip leading "export " if present
        if line.startswith("export "):
            line = line[7:]
        if "=" in line:
            key, _, val = line.partition("=")
            key = key.strip()
            if key and key not in os.environ:
                os.environ[key] = val.strip()


# ── Gamma API: find market ────────────────────────────────────────────────── #

def _extract_token_ids(m: Dict) -> Optional[List[str]]:
    """Pull clobTokenIds from a market dict; returns list or None."""
    token_ids = m.get("clobTokenIds", [])
    if isinstance(token_ids, str):
        try:
            token_ids = json.loads(token_ids)
        except json.JSONDecodeError:
            return None
    return [str(t) for t in token_ids] if token_ids else None


def _nyc_event_slug(date: Optional[Any] = None) -> str:
    """
    Build the Polymarket event slug for the NYC daily high-temperature market.
    Slug pattern: highest-temperature-in-nyc-on-{month}-{d}-{yyyy}
    e.g. highest-temperature-in-nyc-on-february-28-2026
    """
    import datetime as dt
    if date is None:
        date = dt.date.today()
    month = _MONTH_NAMES[date.month - 1]
    return f"highest-temperature-in-nyc-on-{month}-{date.day}-{date.year}"


def find_btc_updown_market(session: requests.Session) -> Optional[Dict]:
    """
    Find the current BTC Up/Down 5m market via Gamma API search.

    Searches for active Bitcoin Up/Down events, picks one with an active CLOB
    orderbook (current 5-minute window). Returns a sub-market dict with
    _yes_token_id, _no_token_id, _neg_risk, _event_title.
    """
    for kw in ["bitcoin up down", "btc up down"]:
        try:
            resp = session.get(
                f"{GAMMA_BASE}/public-search",
                params={
                    "q": kw,
                    "events_status": "active",
                    "limit_per_type": 20,
                    "search_tags": "false",
                    "search_profiles": "false",
                },
                timeout=15,
            )
            resp.raise_for_status()
            data = resp.json()
            events = data.get("events") or []
            if not isinstance(events, list):
                continue

            # Prefer: (1) btc-updown-5m-* (current 5m window), (2) daily bitcoin-up-or-down-on-*
            candidates = []
            for ev in events:
                if not ev.get("active") or ev.get("closed"):
                    continue
                title = (ev.get("title") or "").lower()
                slug = ev.get("slug") or ""
                if ("up" in title and "down" in title) and ("btc" in title or "bitcoin" in title):
                    candidates.append((ev, slug))

            # Sort: 5m first, then daily; within each, by endDate descending (most recent)
            def _score(c):
                ev, slug = c
                if slug.startswith("btc-updown-5m"):
                    return (0, ev.get("endDate") or "")
                if "bitcoin-up-or-down-on-" in slug or "bitcoin-up-or-down-february" in slug:
                    return (1, ev.get("endDate") or "")
                return (2, ev.get("endDate") or "")
            candidates.sort(key=_score)

            # Fetch full event by slug to get markets with token IDs
            for ev, slug in candidates:
                if not slug:
                    continue
                try:
                    er = session.get(f"{GAMMA_BASE}/events", params={"slug": slug}, timeout=10)
                    er.raise_for_status()
                    full = er.json()
                    if isinstance(full, list):
                        full = full[0] if full else {}
                    if not full:
                        continue
                    sub_markets = full.get("markets", [])
                    neg_risk = bool(full.get("negRisk"))
                    event_title = full.get("title", slug)
                    sorted_markets = sorted(
                        sub_markets,
                        key=lambda m: float(m.get("liquidityClob") or m.get("liquidity") or 0),
                        reverse=True,
                    )
                    for m in sorted_markets:
                        tids = _extract_token_ids(m)
                        if not tids:
                            continue
                        try:
                            ck = session.get(
                                f"{CLOB_BASE}/book", params={"token_id": tids[0]}, timeout=5)
                            if "error" in ck.json():
                                continue
                        except Exception:
                            continue
                        m["_yes_token_id"] = tids[0]
                        m["_no_token_id"] = tids[1] if len(tids) > 1 else None
                        m["_neg_risk"] = neg_risk
                        m["_event_title"] = event_title
                        return m
                except Exception:
                    continue
        except Exception:
            continue
    return None


def _nyc_slug_for_next_active(session: requests.Session) -> Optional[str]:
    """Return the slug for today or tomorrow if today's market has ended."""
    import datetime as dt
    for delta in range(3):
        d    = dt.date.today() + dt.timedelta(days=delta)
        slug = _nyc_event_slug(d)
        try:
            resp = session.get(f"{GAMMA_BASE}/events", params={"slug": slug}, timeout=10)
            data = resp.json()
            if isinstance(data, list): data = data[0] if data else {}
            if data.get("active") and not data.get("closed"):
                return slug
        except Exception:
            continue
    return None


def find_nyc_market(session: requests.Session,
                    slug: Optional[str] = None) -> Optional[Dict]:
    """
    Fetch a market from the Gamma API by slug or NYC temperature fallback.

    Lookup order:
      - If slug is provided: try ONLY that slug (no fallback).
      - If slug is None: try NYC date slugs (today±1, today, today+1, today+2),
        then keyword search for "New York temperature".

    Returns a sub-market dict augmented with:
      _yes_token_id, _no_token_id, _neg_risk, _event_title
    """
    import datetime as dt

    slugs_to_try: List[str] = []
    if slug:
        slugs_to_try.append(slug)
    else:
        today = dt.date.today()
        for delta in range(-1, 3):
            slugs_to_try.append(_nyc_event_slug(today + dt.timedelta(days=delta)))

    for s in slugs_to_try:
        try:
            resp = session.get(f"{GAMMA_BASE}/events", params={"slug": s}, timeout=15)
            resp.raise_for_status()
            data = resp.json()
            if isinstance(data, list):
                data = data[0] if data else {}
            if not data:
                continue

            sub_markets = data.get("markets", [])
            neg_risk    = bool(data.get("negRisk"))
            event_title = data.get("title", s)

            # Pick the sub-market with highest liquidityClob that also has an
            # active CLOB orderbook (skips already-resolved sub-markets)
            candidates_sorted = sorted(
                sub_markets,
                key=lambda m: float(m.get("liquidityClob") or m.get("liquidity") or 0),
                reverse=True,
            )
            for m in candidates_sorted:
                tids = _extract_token_ids(m)
                if not tids:
                    continue
                # Quick CLOB check — skip if orderbook doesn't exist
                try:
                    ck = session.get(
                        f"{CLOB_BASE}/book", params={"token_id": tids[0]}, timeout=5)
                    if "error" in ck.json():
                        continue
                except Exception:
                    pass
                m["_yes_token_id"] = tids[0]
                m["_no_token_id"]  = tids[1] if len(tids) > 1 else None
                m["_neg_risk"]     = neg_risk
                m["_event_title"]  = event_title
                return m

        except Exception:
            continue

    # Last-resort: keyword search
    for kw in ["New York temperature", "NYC temperature"]:
        try:
            resp = session.get(
                f"{GAMMA_BASE}/markets",
                params={"keyword": kw, "active": "true", "closed": "false", "limit": 20},
                timeout=15,
            )
            resp.raise_for_status()
            markets = resp.json()
            if not isinstance(markets, list):
                markets = markets.get("markets", markets.get("data", []))
            for m in markets:
                q = m.get("question", "").lower()
                if not (("new york" in q or "nyc" in q) and
                        ("temperature" in q or "temp" in q or "high" in q)):
                    continue
                tids = _extract_token_ids(m)
                if tids:
                    m["_yes_token_id"] = tids[0]
                    m["_no_token_id"]  = tids[1] if len(tids) > 1 else None
                    m["_neg_risk"]     = bool(m.get("negRisk"))
                    m["_event_title"]  = m.get("question", "")
                    return m
        except Exception:
            continue

    return None


# ── WebSocket orderbook snapshot ──────────────────────────────────────────── #

class WsResult:
    def __init__(self):
        self.got_book          = False
        self.asset_id         = ""   # from WS response — must match token_id we subscribed with
        self.best_bid_price   = ""
        self.best_bid_size    = ""
        self.best_ask_price   = ""
        self.best_ask_size    = ""
        self.ping_rtt_ms      = -1.0
        self.delta_count      = 0
        self.delta_ms: List[float] = []
        self.book_to_first_delta_ms = 0.0
        self.full_bids: List  = []
        self.full_asks: List  = []


async def _ws_fetch(token_id: str) -> WsResult:
    result       = WsResult()
    deadline     = time.monotonic() + WS_TIMEOUT_S
    t_book       = None
    t_last_delta = None

    async with websockets.connect(WS_URI, open_timeout=10) as ws:
        sub = json.dumps({"assets_ids": [token_id], "type": "market"})
        await ws.send(sub)

        while time.monotonic() < deadline:
            try:
                remaining = deadline - time.monotonic()
                raw = await asyncio.wait_for(ws.recv(), timeout=min(remaining, 2.0))
            except asyncio.TimeoutError:
                break

            # Polymarket text-level PONG
            if raw == "PONG":
                if _ping_sent_at[0] is not None:
                    result.ping_rtt_ms = (time.monotonic() - _ping_sent_at[0]) * 1000
                    _ping_sent_at[0] = None
                continue

            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                continue

            # Polymarket sends messages as either a JSON object or a JSON array
            msgs = parsed if isinstance(parsed, list) else [parsed]

            for msg in msgs:
                if not isinstance(msg, dict):
                    continue

                mtype = msg.get("type", "")

                # Initial book snapshot: Polymarket sends the bare orderbook
                # object with bids/asks but no "type" field (or type="book").
                is_book = (not result.got_book and
                           ("bids" in msg or "asks" in msg) and
                           mtype in ("", "book", "last_trade_price"))

                if is_book:
                    t_book = time.monotonic()
                    result.got_book = True
                    result.asset_id = str(msg.get("asset_id", ""))
                    bids = msg.get("bids", [])
                    asks = msg.get("asks", [])
                    result.full_bids = sorted(
                        bids, key=lambda x: float(x.get("price", 0)), reverse=True)
                    result.full_asks = sorted(
                        asks, key=lambda x: float(x.get("price", 0)))
                    if result.full_bids:
                        result.best_bid_price = result.full_bids[0].get("price", "")
                        result.best_bid_size  = result.full_bids[0].get("size",  "")
                    if result.full_asks:
                        result.best_ask_price = result.full_asks[0].get("price", "")
                        result.best_ask_size  = result.full_asks[0].get("size",  "")
                    # Send text PING to measure RTT
                    _ping_sent_at[0] = time.monotonic()
                    await ws.send("PING")

                elif mtype == "price_change" and result.got_book:
                    now = time.monotonic()
                    if result.delta_count == 0:
                        result.book_to_first_delta_ms = (now - t_book) * 1000
                        result.delta_ms.append(result.book_to_first_delta_ms)
                    else:
                        result.delta_ms.append((now - t_last_delta) * 1000)
                    t_last_delta = now
                    result.delta_count += 1
                    if result.delta_count >= DELTA_CAP and result.ping_rtt_ms > 0:
                        return result

    return result

# module-level mutable for ping timestamp (avoids closure issues)
_ping_sent_at = [None]


def fetch_orderbook_ws(token_id: str) -> Tuple[WsResult, float]:
    """Run the async WS fetch and return (result, elapsed_ms)."""
    t0  = time.perf_counter()
    res = asyncio.run(_ws_fetch(token_id))
    ms  = (time.perf_counter() - t0) * 1000
    return res, ms


# ── Display helpers ───────────────────────────────────────────────────────── #

def print_orderbook(result: WsResult, top_n: int = 8) -> None:
    def fmt_side(levels: List, label: str) -> None:
        if not levels:
            print(f"    {label}: (empty)")
            return
        print(f"    {label} (price × size):")
        for lvl in levels[:top_n]:
            price = float(lvl.get("price", 0))
            size  = lvl.get("size", "?")
            bar   = "█" * min(int(float(size)) // 10, 30)
            print(f"      ${price:.2f}  ×  {size:<8}  {bar}")

    fmt_side(result.full_bids, "BID")
    fmt_side(result.full_asks, "ASK")


# ── Main ──────────────────────────────────────────────────────────────────── #

def main() -> None:
    _load_dotenv(pathlib.Path(__file__).parent / ".env")

    address  = os.environ.get("POLY_ADDRESS", "")
    eth_priv = os.environ.get("ETH_PRIV_KEY", "")

    can_trade = bool(address and eth_priv)
    if not can_trade:
        print("Note: POLY_ADDRESS / ETH_PRIV_KEY not set — order steps will be skipped.\n")

    # Env overrides: TOKEN_ID skips market search; POLY_EVENT_SLUG picks a specific event
    manual_token_id = os.environ.get("TOKEN_ID", "")
    event_slug      = os.environ.get("POLY_EVENT_SLUG", "").strip()

    session = requests.Session()
    session.headers.update({"Accept": "application/json"})

    # ── Step 1: find market ───────────────────────────────────────────────── #
    print("=" * 60)
    print(" STEP 1 — Find market on Polymarket")
    print("=" * 60)

    neg_risk = False
    if manual_token_id:
        token_id = manual_token_id
        print(f"  Token ID : {token_id[:20]}…  (from TOKEN_ID env var)")
    else:
        # Default: search for current BTC Up/Down via Gamma API
        if not event_slug or event_slug.startswith("btc-updown"):
            market = find_btc_updown_market(session)
        else:
            market = find_nyc_market(session, slug=event_slug)
        if not market:
            print("  No active market found.")
            print("  Tips:")
            print("    TOKEN_ID=<id>           skip search, use this token directly")
            print("    POLY_EVENT_SLUG=<slug>  use specific event (e.g. NYC temp slug)")
            print("    Default: searches Gamma API for current BTC Up/Down 5m market")
            sys.exit(1)

        token_id = market["_yes_token_id"]
        neg_risk = market.get("_neg_risk", False)
        print(f"  Event    : {market.get('_event_title', market.get('question', ''))}")
        print(f"  Question : {market.get('question', '')}")
        print(f"  Token ID : {token_id[:20]}…")
        print(f"  NegRisk  : {neg_risk}")
        try:
            print(f"  Outcome  : ${float(market.get('outcomePrices', '[0]')[1:-1].split(',')[0]):.2f} YES")
        except Exception:
            pass
        print(f"  Closes   : {market.get('endDate', market.get('close_time', '—'))}")

    # ── Step 2: WebSocket orderbook snapshot ──────────────────────────────── #
    print(f"\n{'=' * 60}")
    print(f" STEP 2 — Orderbook via WebSocket")
    print("=" * 60)

    ws_result, t_orderbook = fetch_orderbook_ws(token_id)

    if not ws_result.got_book:
        print("  (no book snapshot received)")
    else:
        # Verify WebSocket orderbook matches the token we're trading
        ws_asset = ws_result.asset_id or ""
        if ws_asset and ws_asset != token_id:
            print(f"  [ERROR] Orderbook mismatch: subscribed to token_id {token_id[:20]}…")
            print(f"          but received book for asset_id {ws_asset[:20]}…")
            sys.exit(1)
        # Cross-check with CLOB REST API
        try:
            r = session.get(f"{CLOB_BASE}/book", params={"token_id": token_id}, timeout=5)
            if r.ok:
                rb = r.json()
                rest_bids = rb.get("bids", [])
                rest_best_bid = float(rest_bids[0]["price"]) if rest_bids else 0.0
                ws_best_bid = float(ws_result.best_bid_price) if ws_result.best_bid_price else 0.0
                if abs(rest_best_bid - ws_best_bid) > 0.001:
                    print(f"  [WARN] REST best bid {rest_best_bid:.4f} ≠ WS best bid {ws_best_bid:.4f}")
                else:
                    print(f"  Orderbook verified (token_id {token_id[:16]}…, REST↔WS match)")
        except Exception as e:
            print(f"  [WARN] REST cross-check failed: {e}")
        print_orderbook(ws_result)

    print(f"  Snapshot latency (incl. TLS) : {t_orderbook:.1f} ms")
    if ws_result.ping_rtt_ms > 0:
        print(f"  Ping/pong RTT               : {ws_result.ping_rtt_ms:.1f} ms"
              f"  ({ws_result.ping_rtt_ms / 2:.1f} ms one-way est.)")
    else:
        print("  Ping/pong RTT               : (no pong received)")

    if ws_result.delta_count > 0:
        print(f"  Book → first price_change   : {ws_result.book_to_first_delta_ms:.1f} ms")
        print("  price_change inter-arrivals :")
        for i, ms in enumerate(ws_result.delta_ms):
            tag = "  ← book → first delta" if i == 0 else ""
            print(f"    [{i+1}] {ms:7.1f} ms{tag}")
    else:
        print("  price_change                : (none in window — market quiet)")

    # ── Steps 3 & 4: place + cancel ───────────────────────────────────────── #
    t_place  = 0.0
    t_sign   = 0.0
    t_post   = 0.0
    t_cancel = 0.0
    order_id = None

    # Only place order if best bid >= 30¢ (ensures liquidity; our 1¢ order will rest)
    best_bid = 0.0
    if ws_result.best_bid_price:
        try:
            best_bid = float(ws_result.best_bid_price)
        except (ValueError, TypeError):
            pass
    min_bid_to_place = 0.30  # 30¢

    if not can_trade:
        print(f"\n{'=' * 60}")
        print(" STEPS 3+4 — Skipped (POLY_ADDRESS / ETH_PRIV_KEY not set)")
        print("=" * 60)
    elif best_bid < min_bid_to_place:
        print(f"\n{'=' * 60}")
        print(f" STEPS 3+4 — Skipped (best bid {best_bid*100:.0f}¢ < {min_bid_to_place*100:.0f}¢ threshold)")
        print("=" * 60)
    else:
        # ── Step 3: place order (REST via py-clob-client) ──────────────────── #
        print(f"\n{'=' * 60}")
        print(f" STEP 3 — Place limit buy (100 tokens @ $0.01) [best bid {best_bid*100:.0f}¢ ≥ {min_bid_to_place*100:.0f}¢]")
        print("=" * 60)

        client = ClobClient(
            host=CLOB_BASE,
            chain_id=CHAIN_ID,
            key=eth_priv if eth_priv.startswith("0x") else "0x" + eth_priv,
            signature_type=SIG_TYPE,
            funder=address,
        )
        client.set_api_creds(client.create_or_derive_api_creds())

        order_args = OrderArgs(token_id=token_id, price=0.01, size=100.0, side=BUY)
        opts = PartialCreateOrderOptions(neg_risk=neg_risk)

        t0 = time.perf_counter()
        try:
            signed = client.create_order(order_args, opts)
        except Exception as e:
            signed = None
            pr = {"success": False, "errorMsg": str(e)}
        t_sign = (time.perf_counter() - t0) * 1000

        if signed is not None:
            t1 = time.perf_counter()
            try:
                pr = client.post_order(signed, OrderType.GTC)
            except Exception as e:
                pr = {"success": False, "errorMsg": str(e)}
            t_post = (time.perf_counter() - t1) * 1000
        else:
            t_post = 0.0
        t_place = t_sign + t_post

        # post_order returns object with success, orderID, etc.
        if hasattr(pr, "success"):
            success = pr.success
            order_id = getattr(pr, "orderID", None) or getattr(pr, "id", None)
        else:
            pr = pr if isinstance(pr, dict) else {}
            success = pr.get("success", False)
            order_id = pr.get("orderID") or pr.get("id")

        print(f"  Success  : {success}")
        print(f"  Order ID : {order_id or '(not returned)'}")
        if not success:
            err = getattr(pr, "errorMsg", None) or (pr.get("errorMsg") if isinstance(pr, dict) else None) or pr.get("error", "")
            if err:
                print(f"  Error    : {err}")
            elif isinstance(pr, dict):
                print(f"  Response : {json.dumps(pr)[:400]}")
        print(f"  Signing  : {t_sign:.1f} ms  (EIP-712 client)")
        print(f"  Post     : {t_post:.1f} ms  (network + server verify + insert)")
        print(f"  Latency  : {t_place:.1f} ms  (total)")

        # ── Step 4: cancel order (REST via py-clob-client) ─────────────────── #
        if order_id:
            print(f"\n{'=' * 60}")
            print(f" STEP 4 — Cancel order {order_id}")
            print("=" * 60)

            t0 = time.perf_counter()
            try:
                cr = client.cancel(order_id)
            except Exception as e:
                cr = {"cancelledIDs": [], "cancelErrors": {str(order_id): str(e)}}
            t_cancel = (time.perf_counter() - t0) * 1000

            cr_dict = cr if isinstance(cr, dict) else (cr.model_dump() if hasattr(cr, "model_dump") else {})
            print(f"  Response : {json.dumps(cr_dict)[:120]}")
            print(f"  Latency  : {t_cancel:.1f} ms")
        else:
            print(f"\n{'=' * 60}")
            print(" STEP 4 — Skipped (no order ID returned from place)")
            print("=" * 60)

    # ── Timing summary ────────────────────────────────────────────────────── #
    print(f"\n{'=' * 60}")
    print(" TIMING SUMMARY")
    print("=" * 60)
    print(f"  Orderbook snapshot (WSS) : {t_orderbook:>8.1f} ms  (incl. TLS handshake)")
    if ws_result.ping_rtt_ms > 0:
        print(f"  Per-msg RTT (ping/pong)  : {ws_result.ping_rtt_ms:>8.1f} ms"
              f"  ({ws_result.ping_rtt_ms / 2:.1f} ms one-way)")
    if can_trade:
        print(f"  Place order  (REST)      : {t_place:>8.1f} ms")
        if t_sign > 0 or t_post > 0:
            print(f"    ├─ Signing (EIP-712)   : {t_sign:>8.1f} ms")
            print(f"    └─ Post (net+verify)  : {t_post:>8.1f} ms")
        if t_cancel > 0:
            print(f"  Cancel order (REST)      : {t_cancel:>8.1f} ms")
    print(f"  {'─' * 40}")
    total = t_orderbook + t_place + t_cancel
    print(f"  Total                    : {total:>8.1f} ms")
    print("\nDone.")


if __name__ == "__main__":
    main()
