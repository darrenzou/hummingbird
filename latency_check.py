"""
nyc_weather_order.py

######################################
####### NOTE: API credentials not included in this file #######
####### To run this script, you need to create a .env file in the same directory as this script #######
####### The .env file should contain the following variables: #######
####### KALSHI_API_KEY_ID: The API key ID from Kalshi #######
####### KALSHI_PRIVATE_KEY_PEM: The PEM block of the RSA private key #######
####### KALSHI_PRIVATE_KEY: The path to the .key file #######
####### KALSHI_BASE_URL: The base URL to use for the API #######
####### KALSHI_DEMO: Set to "1" to point at demo environment #######

1. Finds today's active NYC high-temperature market on Kalshi (HIGHNY series).
2. Batch creates two limit orders at 1¢ and 3¢ (vol 1).
3. Repeatedly amends both: price -> (price % 3) + 1 until error.
4. Cancels both orders and reports timing (batch create, amend latency).

Credentials are loaded from .env (git-ignored). Keys read:
  KALSHI_API_KEY_ID        – API Key ID UUID
  KALSHI_PRIVATE_KEY_PEM   – PEM block of the RSA private key (multi-line OK)
  KALSHI_PRIVATE_KEY       – Path to PEM/.key file (alternative to PEM block)
  KALSHI_PRIVATE_KEY_PATH  – Same as KALSHI_PRIVATE_KEY (path to PEM file)
  KALSHI_BASE_URL          – override base URL (default: production)
  KALSHI_DEMO              – set to "1" to point at demo environment

  .env may use "export KEY=value" or "KEY=value" lines.

Usage:
  python nyc_weather_order.py
"""

import base64
import collections
import dataclasses
import datetime
import json
import os
import pathlib
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional
from urllib.parse import urlparse

import requests
import websocket  # pip install websocket-client
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

# ──────────────────────────────────────────────────────────────────────────── #
# Config                                                                       #
# ──────────────────────────────────────────────────────────────────────────── #

PROD_BASE  = "https://api.elections.kalshi.com/trade-api/v2"
DEMO_BASE  = "https://demo-api.kalshi.co/trade-api/v2"
PROD_WS    = "wss://api.elections.kalshi.com/trade-api/ws/v2"
DEMO_WS    = "wss://demo-api.kalshi.co/trade-api/ws/v2"

WS_TIMEOUT_S    = 10   # seconds to wait for orderbook_snapshot
MAX_DELTAS      = 10   # collect exactly this many deltas after the snapshot
DELTA_TIMEOUT_S = 60.0 # hard deadline (seconds) after snapshot — ensures we always
                        # wait long enough to collect all MAX_DELTAS even on quiet markets


@dataclass
class WsTimings:
    """Timing measurements collected during a WebSocket orderbook session."""
    snapshot_ms: float = 0.0
    # Time between consecutive messages (snapshot→delta[0], delta[0]→delta[1], …)
    delta_inter_arrival_ms: List[float] = field(default_factory=list)
    # Pure Python time to apply each delta to the in-memory orderbook
    delta_parse_ms: List[float] = field(default_factory=list)
    delta_apply_ms: List[float] = field(default_factory=list)

# Ticker format: KXHIGHNY-{YY}{MON}{DD}  e.g. KXHIGHNY-26FEB24 = Feb 24 2026
# (Kalshi website URLs are lowercased; the API uses uppercase tickers)
NYC_SERIES = "KXEUROVISION-26-FIN"

# Guard rails: refuse to place orders when market is at extremes
MIN_YES_BID = 6   # best yes bid must be > 5
MAX_NO_BID  = 95  # best no bid must be < 96

ORDER_SIDE      = "yes"
ORDER_ACTION    = "buy"
ORDER_COUNT     = 1
ORDER_YES_PRICE = 1          # 1¢ — safely below any real market so it rests


# ──────────────────────────────────────────────────────────────────────────── #
# .env loader (no third-party dependency)                                      #
# ──────────────────────────────────────────────────────────────────────────── #

def _load_dotenv(env_path: pathlib.Path) -> None:
    """
    Parse a .env file and inject values into os.environ (existing env vars
    take precedence, consistent with standard dotenv behaviour).

    Supports multi-line values delimited by a blank line:
        KEY=line one
        line two
        <blank line ends the value>
    """
    if not env_path.exists():
        return

    text = env_path.read_text(encoding="utf-8")
    current_key: Optional[str] = None
    current_lines: List[str] = []

    def _flush() -> None:
        if current_key and current_key not in os.environ:
            os.environ[current_key] = "\n".join(current_lines)

    for raw_line in text.splitlines():
        # blank line ends a multi-line value
        if raw_line.strip() == "":
            if current_key:
                _flush()
                current_key = None
                current_lines = []
            continue

        # skip comment lines
        if raw_line.lstrip().startswith("#"):
            continue

        if current_key is None:
            # look for KEY=value (allow "export KEY=value" like in shell .env)
            if "=" in raw_line:
                key, _, value = raw_line.partition("=")
                key = key.strip()
                if key.startswith("export "):
                    key = key[7:].strip()
                current_key = key
                current_lines = [value]  # first line of value (may be empty)
            # lines before any key are ignored
        else:
            # Detect a new KEY=value line vs. a continuation of a multi-line value.
            # A new entry starts with an identifier (letters/digits/underscores)
            # followed by '=', optionally prefixed with 'export '.
            candidate = raw_line.strip()
            if candidate.startswith("export "):
                candidate = candidate[7:].strip()
            eq_pos = candidate.find("=")
            if eq_pos > 0 and candidate[:eq_pos].replace("_", "").isalnum():
                _flush()
                key, _, value = raw_line.partition("=")
                key = key.strip()
                if key.startswith("export "):
                    key = key[7:].strip()
                current_key = key
                current_lines = [value]
            else:
                current_lines.append(raw_line)

    _flush()  # flush last key if file doesn't end with blank line


# ──────────────────────────────────────────────────────────────────────────── #
# Auth helpers                                                                 #
# ──────────────────────────────────────────────────────────────────────────── #

def _load_private_key_from_file(path: str):
    with open(path, "rb") as f:
        return serialization.load_pem_private_key(
            f.read(), password=None, backend=default_backend()
        )


def _load_private_key_from_pem(pem: str):
    return serialization.load_pem_private_key(
        pem.encode(), password=None, backend=default_backend()
    )


def _sign(private_key, timestamp: str, method: str, path: str) -> str:
    """RSA-PSS SHA-256 signature over '{timestamp}{METHOD}{path_no_query}'."""
    path_clean = path.split("?")[0]
    message = f"{timestamp}{method}{path_clean}".encode()
    sig = private_key.sign(
        message,
        padding.PSS(
            mgf=padding.MGF1(hashes.SHA256()),
            salt_length=hashes.SHA256().digest_size,
        ),
        hashes.SHA256(),
    )
    return base64.b64encode(sig).decode()


def _auth_headers(private_key, api_key_id: str, method: str, path: str) -> Dict[str, str]:
    # Sanitize values used in HTTP headers to avoid stray newlines/spaces.
    ts = str(int(datetime.datetime.now().timestamp() * 1000))
    clean_api_key = (api_key_id or "").strip().replace("\r", "").replace("\n", "")
    signature = _sign(private_key, ts, method, path).replace("\r", "").replace("\n", "")
    return {
        "KALSHI-ACCESS-KEY":       clean_api_key,
        "KALSHI-ACCESS-TIMESTAMP": ts,
        "KALSHI-ACCESS-SIGNATURE": signature,
        "Content-Type":            "application/json",
        "Accept":                  "application/json",
    }


# ──────────────────────────────────────────────────────────────────────────── #
# Rate limiter                                                                 #
# ──────────────────────────────────────────────────────────────────────────── #

class RateLimiter:
    """
    Token-bucket rate limiter. Thread-safe.

    Allows up to `max_calls` requests per `period` seconds.
    Callers invoke `acquire()` which blocks until a token is available.
    """

    def __init__(self, max_calls: int, period: float = 1.0):
        self.max_calls = max_calls
        self.period    = period
        self._lock     = threading.Lock()
        self._timestamps: collections.deque = collections.deque()

    def acquire(self) -> None:
        with self._lock:
            now = time.monotonic()
            # drop timestamps outside the current window
            cutoff = now - self.period
            while self._timestamps and self._timestamps[0] <= cutoff:
                self._timestamps.popleft()

            if len(self._timestamps) >= self.max_calls:
                # sleep until the oldest token falls outside the window
                sleep_for = self.period - (now - self._timestamps[0])
                if sleep_for > 0:
                    time.sleep(sleep_for)

            self._timestamps.append(time.monotonic())


# ──────────────────────────────────────────────────────────────────────────── #
# API wrappers                                                                 #
# ──────────────────────────────────────────────────────────────────────────── #

MAX_RETRIES    = 5
RETRY_BACKOFF  = 2.0   # base seconds; doubles each attempt


class KalshiClient:
    def __init__(self, api_key_id: str, private_key, base_url: str,
                 rate_limit: int = 20):
        self.api_key_id  = api_key_id
        self.private_key = private_key
        self.base        = base_url.rstrip("/")
        self.session     = requests.Session()
        self._limiter    = RateLimiter(max_calls=rate_limit, period=1.0)
        # Path prefix used in RSA signatures: everything after the hostname
        # e.g. "https://api.elections.kalshi.com/trade-api/v2" → "/trade-api/v2"
        self._api_prefix = urlparse(self.base).path

    # ── internal ──────────────────────────────────────────────────────────── #

    def _request(self, method: str, path: str,
                 params: Optional[Dict] = None,
                 body:   Optional[Dict] = None,
                 auth:   bool = True) -> Any:
        """
        Execute an HTTP request with rate-limiting and automatic 429 retry.
        Respects the Retry-After header when present; otherwise uses
        exponential backoff starting at RETRY_BACKOFF seconds.
        """
        url = self.base + path
        # Signature must cover the full path including the /trade-api/v2 prefix
        sign_path = self._api_prefix + path

        for attempt in range(MAX_RETRIES):
            self._limiter.acquire()
            hdrs = _auth_headers(self.private_key, self.api_key_id, method, sign_path) if auth else {}

            resp = self.session.request(
                method, url, headers=hdrs, params=params, json=body, timeout=30
            )

            if resp.status_code == 429:
                retry_after = float(resp.headers.get("Retry-After", RETRY_BACKOFF * (2 ** attempt)))
                print(f"  [rate limit] 429 received — waiting {retry_after:.1f}s "
                      f"(attempt {attempt + 1}/{MAX_RETRIES})")
                time.sleep(retry_after)
                continue

            # Extra debugging for auth/permission issues.
            if resp.status_code in (401, 403):
                print("\n  [auth debug] HTTP", resp.status_code, method, path)
                print("    URL          :", url)
                print("    api_key_id   :", (self.api_key_id or "")[:8] + "...")
                print("    has_private_key:", bool(self.private_key))
                try:
                    err_json = resp.json()
                except ValueError:
                    err_json = None
                if err_json:
                    print("    error_json   :", err_json)
                else:
                    print("    error_body   :", (resp.text or "").strip()[:500])

            resp.raise_for_status()
            return resp.json()

        raise RuntimeError(f"Request failed after {MAX_RETRIES} attempts: {method} {path}")

    def _get(self, path: str, params: Optional[Dict] = None, auth: bool = True) -> Any:
        return self._request("GET", path, params=params, auth=auth)

    def _post(self, path: str, body: Dict) -> Any:
        return self._request("POST", path, body=body)

    def _delete(self, path: str) -> Any:
        return self._request("DELETE", path)

    # ── public ────────────────────────────────────────────────────────────── #

    def get_market(self, ticker: str) -> Dict:
        """Fetch a single market by its exact ticker."""
        data = self._get(f"/markets/{ticker}")
        return data.get("market", data)

    def get_markets_by_series(self, series_ticker: str, status: str = "open",
                              limit: int = 100) -> List[Dict]:
        """Fetch markets filtered by series ticker."""
        data = self._get("/markets", params={
            "series_ticker": series_ticker,
            "status":        status,
            "limit":         limit,
        })
        if isinstance(data, list):
            return data
        for key in ("markets", "data", "results"):
            if key in data and isinstance(data[key], list):
                return data[key]
        return []

    def get_orderbook(self, ticker: str, depth: int = 0) -> Dict:
        """
        Returns the orderbook in a legacy-friendly format (no auth required).

        Kalshi's v2 API now returns fixed-point orderbooks under
        `orderbook_fp.yes_dollars` and `orderbook_fp.no_dollars`, where each
        level is ["price_dollars", "count_fp"] as strings.

        This adapter converts that structure into:
            {
              "yes": [[price_cents, qty_int], ...],
              "no":  [[price_cents, qty_int], ...],
              ...any other passthrough fields...
            }
        so that existing code expecting integer cents and quantities continues
        to work.
        """
        data = self._get(
            f"/markets/{ticker}/orderbook",
            params={"depth": depth},
            auth=False,
        )
        # New structure: {"orderbook_fp": {"yes_dollars": [...], "no_dollars": [...]} }
        if "orderbook_fp" in data:
            ob_fp = data["orderbook_fp"] or {}

            def _convert_side(levels: Any) -> list[list[int]]:
                out: list[list[int]] = []
                if not levels:
                    return out
                for price_str, count_str in levels:
                    # price_dollars -> integer cents (e.g. "0.4200" -> 42)
                    try:
                        price_cents = int(round(float(price_str) * 100))
                    except (TypeError, ValueError):
                        continue
                    # count_fp is a fixed-point string; we keep the integer part.
                    try:
                        qty = int(float(count_str))
                    except (TypeError, ValueError):
                        qty = 0
                    out.append([price_cents, qty])
                return out

            converted = {
                "yes": _convert_side(ob_fp.get("yes_dollars")),
                "no":  _convert_side(ob_fp.get("no_dollars")),
            }
            # Preserve original payload for debugging if needed.
            converted["_raw_orderbook_fp"] = ob_fp
            return converted

        # Backwards compatibility: older structure already in expected format.
        return data.get("orderbook", data)

    def create_order(
        self,
        ticker: str,
        side: str,
        action: str,
        count: int,
        yes_price: int,
        time_in_force: str = "good_till_canceled",
    ) -> Dict:
        body = {
            "ticker":         ticker,
            "side":           side,
            "action":         action,
            "count":          count,
            "yes_price":      yes_price,
            "time_in_force":  time_in_force,
            "client_order_id": str(uuid.uuid4()),
        }
        return self._post("/portfolio/orders", body)

    def batch_create_orders(
        self,
        orders: List[Dict],
    ) -> Dict:
        """Create multiple orders in one request. Up to 20 orders per batch."""
        body = {"orders": orders}
        return self._post("/portfolio/orders/batched", body)

    def amend_order(
        self,
        order_id: str,
        ticker: str,
        side: str,
        action: str,
        count: Optional[int] = None,
        yes_price: Optional[int] = None,
        client_order_id: Optional[str] = None,
    ) -> Dict:
        """Amend an existing order's price and/or size."""
        body: Dict[str, Any] = {
            "ticker": ticker,
            "side":   side,
            "action": action,
        }
        if count is not None:
            body["count"] = count
        if yes_price is not None:
            body["yes_price"] = yes_price
        if client_order_id is not None:
            body["client_order_id"] = client_order_id
        return self._post(f"/portfolio/orders/{order_id}/amend", body)

    def get_order(self, order_id: str) -> Dict:
        """Fetch a single order by its ID."""
        data = self._get(f"/portfolio/orders/{order_id}")
        return data.get("order", data)

    def cancel_order(self, order_id: str) -> Dict:
        return self._delete(f"/portfolio/orders/{order_id}")


# ──────────────────────────────────────────────────────────────────────────── #
# Orderbook delta application                                                  #
# ──────────────────────────────────────────────────────────────────────────── #

def _apply_ob_delta(ob: Dict, delta_msg: Dict) -> None:
    """
    Apply a Kalshi orderbook_delta message in-place to `ob`.

    Each entry in delta_msg["yes"] / delta_msg["no"] is [price, new_qty].
    - new_qty > 0  → set (or add) that price level to new_qty
    - new_qty == 0 → remove that price level entirely
    """
    for side in ("yes", "no"):
        changes = delta_msg.get(side, [])
        if not changes:
            continue
        levels: Dict[int, int] = {entry[0]: entry[1] for entry in ob.get(side, [])}
        for price, qty in changes:
            if qty == 0:
                levels.pop(price, None)
            else:
                levels[price] = qty
        ob[side] = [[p, q] for p, q in levels.items()]


# ──────────────────────────────────────────────────────────────────────────── #
# WebSocket orderbook                                                          #
# ──────────────────────────────────────────────────────────────────────────── #

def fetch_orderbook_ws(
    private_key, api_key_id: str, ws_url: str, ticker: str
) -> tuple:
    """
    Connect to the Kalshi WebSocket, subscribe to orderbook_delta, receive the
    snapshot, then stay connected to collect up to MAX_DELTAS delta messages.

    For each delta we measure:
      inter_arrival_ms  — wall time from the previous message arriving to this
                          one arriving (network + server cadence)
      parse_ms          — time to json.loads() + extract the delta payload
      apply_ms          — time to apply the delta to the in-memory orderbook

    Returns (orderbook_dict, WsTimings).
    """
    ws_path = urlparse(ws_url).path

    ts  = str(int(datetime.datetime.now().timestamp() * 1000))
    sig = _sign(private_key, ts, "GET", ws_path)

    header = [
        f"KALSHI-ACCESS-KEY: {api_key_id}",
        f"KALSHI-ACCESS-TIMESTAMP: {ts}",
        f"KALSHI-ACCESS-SIGNATURE: {sig}",
    ]

    ob: Dict      = {}
    timings       = WsTimings()
    done          = threading.Event()
    t_connect     = time.perf_counter()

    # Mutable state shared across callbacks (dict avoids nonlocal on primitives)
    state = {
        "snapshot_done": False,
        "t_last_msg":    0.0,   # perf_counter when the last message arrived
        "deadline":      0.0,   # perf_counter after which we stop collecting
    }

    def on_open(ws):
        sub = {
            "id":  1,
            "cmd": "subscribe",
            "params": {
                "channels":      ["orderbook_delta"],
                "market_ticker": ticker,
            },
        }
        ws.send(json.dumps(sub))

    def on_message(ws, message):
        # Record arrival time before any processing
        t_recv = time.perf_counter()

        # ── Parse ──────────────────────────────────────────────────────────
        t_parse_start = time.perf_counter()
        msg      = json.loads(message)
        msg_type = msg.get("type")
        payload  = msg.get("msg", {})
        t_parse_done = time.perf_counter()

        # ── Snapshot ───────────────────────────────────────────────────────
        if msg_type == "orderbook_snapshot":
            ob.update(payload)
            timings.snapshot_ms       = (t_recv - t_connect) * 1000
            state["snapshot_done"]    = True
            state["t_last_msg"]       = t_recv
            state["deadline"]         = t_recv + DELTA_TIMEOUT_S
            return

        # ── Delta ──────────────────────────────────────────────────────────
        if msg_type != "orderbook_delta" or not state["snapshot_done"]:
            return

        inter_arrival = (t_recv - state["t_last_msg"]) * 1000
        parse_ms      = (t_parse_done - t_parse_start) * 1000

        t_apply_start = time.perf_counter()
        _apply_ob_delta(ob, payload)
        t_apply_done  = time.perf_counter()
        apply_ms      = (t_apply_done - t_apply_start) * 1000

        timings.delta_inter_arrival_ms.append(inter_arrival)
        timings.delta_parse_ms.append(parse_ms)
        timings.delta_apply_ms.append(apply_ms)

        state["t_last_msg"] = t_recv

        if (len(timings.delta_inter_arrival_ms) >= MAX_DELTAS
                or t_recv >= state["deadline"]):
            done.set()
            ws.close()

    def on_error(ws, error):
        print(f"  [ws] error: {error}")
        done.set()

    def on_close(ws, code, reason):
        done.set()

    ws_app = websocket.WebSocketApp(
        ws_url,
        header=header,
        on_open=on_open,
        on_message=on_message,
        on_error=on_error,
        on_close=on_close,
    )

    t = threading.Thread(target=lambda: ws_app.run_forever(ping_interval=0))
    t.daemon = True
    t.start()

    done.wait(timeout=WS_TIMEOUT_S + DELTA_TIMEOUT_S)
    return ob, timings


# ──────────────────────────────────────────────────────────────────────────── #
# Market lookup                                                                #
# ──────────────────────────────────────────────────────────────────────────── #

def find_todays_nyc_market(client: KalshiClient) -> Optional[Dict]:
    """
    Locate the target market to use for latency checks.

    Behaviour:
      - If NYC_SERIES looks like a full ticker (e.g. "KXEUROVISION-26-FIN"),
        perform a direct GET /markets/{ticker}.
      - Otherwise, treat NYC_SERIES as a series_ticker and pick the first
        market in that series with yes_bid > 5¢ (original behaviour).
    """
    series_or_ticker = NYC_SERIES.strip()

    # If this looks like a concrete ticker (contains a date-like suffix), hit
    # the single-market endpoint. This matches how other scripts target
    # specific tickers like KXEUROVISION-26-FIN.
    if "-" in series_or_ticker:
        print(f"  Direct market lookup: ticker={series_or_ticker}")
        try:
            market = client.get_market(series_or_ticker)
        except requests.HTTPError as e:
            print(f"  [error] GET /markets/{series_or_ticker} failed: {e}")
            body = e.response.text if getattr(e, "response", None) is not None else "no body"
            print(f"  Body: {body[:500]}")
            return None

        if not market:
            print(f"  No market returned for ticker {series_or_ticker}.")
            return None

        print(f"  Found: {market.get('ticker')}  (direct ticker lookup)")
        return market

    # Fallback: original series-based behaviour (e.g. KXHIGHNY)
    print(f"  Series search: series_ticker={series_or_ticker}")
    markets = client.get_markets_by_series(series_or_ticker)
    if not markets:
        return None

    chosen = None
    for m in markets:
        yes_bid = m.get("yes_bid")
        if yes_bid is not None and yes_bid > 5:
            chosen = m
            break

    if not chosen:
        print("  No market in series has yes_bid > 5¢; aborting.")
        return None

    print(
        f"  Found: {chosen.get('ticker')}  (yes_bid={chosen.get('yes_bid')}¢, {len(markets)} market(s) in series)"
    )
    return chosen


def check_guard_rails(client: KalshiClient, ticker: str) -> tuple[bool, Optional[str]]:
    """
    Ensure market is within safe bounds before placing orders.
    Returns (True, None) if OK, else (False, reason).

    Newer markets may not expose top-of-book bids directly on the market
    object, so we derive best YES/NO bids from the live orderbook instead.
    """
    try:
        ob = client.get_orderbook(ticker)
    except requests.HTTPError as e:
        return False, f"Failed to fetch orderbook for {ticker}: {e}"

    def _best_bid(levels: Optional[List[List[int]]]) -> Optional[int]:
        if not levels:
            return None
        # Each level is [price, qty]; API returns ascending by price.
        return int(levels[-1][0])

    yes_levels = ob.get("yes") or []
    no_levels  = ob.get("no") or []

    yes_bid = _best_bid(yes_levels)
    no_bid  = _best_bid(no_levels)

    if yes_bid is None and no_bid is None:
        return False, "Orderbook missing both YES and NO bids"

    if yes_bid is not None and yes_bid <= MIN_YES_BID - 1:
        return False, f"yes_bid {yes_bid}¢ ≤ {MIN_YES_BID - 1} (guard: best bid > 5)"

    if no_bid is not None and no_bid >= MAX_NO_BID + 1:
        return False, f"no_bid {no_bid}¢ ≥ {MAX_NO_BID + 1} (guard: best no bid < 96)"

    return True, None


# ──────────────────────────────────────────────────────────────────────────── #
# Display helpers                                                              #
# ──────────────────────────────────────────────────────────────────────────── #

def print_market(m: Dict) -> None:
    print("  Ticker     :", m.get("ticker"))
    print("  Title      :", m.get("title", m.get("question", "—")))
    print("  Yes ask    :", m.get("yes_ask"), "¢")
    print("  No ask     :", m.get("no_ask"), "¢")
    print("  Closes     :", m.get("close_time", m.get("expiration_time", "—")))


def print_orderbook(ob: Dict) -> None:
    def fmt_side(levels: List, label: str) -> None:
        if not levels:
            print(f"    {label}: (empty)")
            return
        # levels are [price, qty] sorted ascending; best bid is last
        sorted_levels = sorted(levels, key=lambda x: x[0], reverse=True)
        print(f"    {label} bids (price¢ × qty):")
        for price, qty in sorted_levels[:10]:
            bar = "█" * min(qty // 10, 30)
            print(f"      {price:>3}¢  ×  {qty:<6}  {bar}")

    fmt_side(ob.get("yes", []), "YES")
    fmt_side(ob.get("no",  []), "NO")


def print_order(label: str, order: Dict) -> None:
    print(f"  Order ID   : {order.get('order_id')}")
    print(f"  Status     : {order.get('status')}")
    print(f"  Side       : {order.get('side')}  {order.get('action')}")
    print(f"  Yes price  : {order.get('yes_price')}¢")
    print(f"  Count      : {order.get('initial_count')}")


# ──────────────────────────────────────────────────────────────────────────── #
# Batch create + modify loop                                                   #
# ──────────────────────────────────────────────────────────────────────────── #

def _next_price(current: int) -> int:
    """Cycle price: 3->1, 2->3, 1->2."""
    return (current % 3) + 1


def run_batch_create_and_modify(client: KalshiClient, ticker: str,
                                market: Optional[Dict] = None) -> None:
    """
    Create two limit orders at 1¢ and 3¢ (vol 1) via single create_order
    (not batch), then repeatedly amend each to new price = (current % 3) + 1
    until an error. Uses single-create to avoid batch endpoint 404 on amend.
    Reports timing: create (2 orders), and per-amend latency.
    """
    p_lo, p_hi = 1, 3

    print("\n" + "=" * 60)
    print(" Create (single) + modify loop (1¢ and 3¢, price=(p%3)+1)")
    print("=" * 60)
    print("  Creating 2 orders via create_order (not batch): 1¢ and 3¢ YES buy")
    print("  Amending both in a loop: 3->1, 2->3, 1->2")
    print("=" * 60)

    try:
        t0 = time.perf_counter()
        resp1 = client.create_order(ticker, ORDER_SIDE, ORDER_ACTION, 1, p_lo)
        resp2 = client.create_order(ticker, ORDER_SIDE, ORDER_ACTION, 1, p_hi)
        t_create_ms = (time.perf_counter() - t0) * 1000
    except requests.HTTPError as e:
        print(f"  [error] Create failed: {e}")
        print(f"  Response: {e.response.text if e.response else 'no body'}")
        return

    print(f"  Create (2 orders): {t_create_ms:.1f} ms")

    o1 = resp1.get("order", resp1)
    o2 = resp2.get("order", resp2)
    id1 = o1.get("order_id")
    id2 = o2.get("order_id")
    cid1 = o1.get("client_order_id")
    cid2 = o2.get("client_order_id")
    if not id1 or not id2:
        print(f"  [error] Missing order_id: {o1}, {o2}")
        return

    # Debug: query the orders back from Kalshi immediately
    print("\n  Verifying orders via GET /portfolio/orders/{order_id}:")
    for label, oid in (("1", id1), ("2", id2)):
        try:
            live = client.get_order(oid)
            print(
                f"    Order {label} {oid}: "
                f"status={live.get('status')} "
                f"side={live.get('side')} "
                f"action={live.get('action')} "
                f"yes_price={live.get('yes_price')} "
                f"initial={live.get('initial_count')} "
                f"remaining={live.get('remaining_count')} "
                f"fill={live.get('fill_count')}"
            )
        except requests.HTTPError as e:
            body = e.response.text if getattr(e, "response", None) is not None else "no body"
            print(f"    [error] GET order {oid}: {e} | body={body[:300]}")

    p1, p2 = p_lo, p_hi
    amend_ms: List[float] = []
    last_error = None

    print(f"  Order 1: {id1} @ {p1}¢")
    print(f"  Order 2: {id2} @ {p2}¢")
    print("  Modifying price (p -> p%3+1) for 20 cycles...")

    for n in range(20):
        try:
            p1_new = (p1 % 3) + 1
            t0 = time.perf_counter()
            resp = client.amend_order(
                order_id=id1,
                ticker=ticker,
                side=ORDER_SIDE,
                action=ORDER_ACTION,
                count=1,
                yes_price=p1_new,
            )
            amend_ms.append((time.perf_counter() - t0) * 1000)
            order_after = resp.get("order", resp)
            if order_after:
                print(
                    f"    Amend1[{n+1}]: id={id1} old_p={p1} new_p={p1_new} "
                    f"status={order_after.get('status')} "
                    f"remaining={order_after.get('remaining_count')} "
                    f"fill={order_after.get('fill_count')}"
                )
                id1 = order_after.get("order_id") or id1
                if order_after.get("status") == "executed":
                    last_error = RuntimeError("Order 1 filled (status=executed)")
                    break
            p1 = p1_new
        except requests.HTTPError as e:
            last_error = e
            break

        try:
            p2_new = (p2 % 3) + 1
            t0 = time.perf_counter()
            resp = client.amend_order(
                order_id=id2,
                ticker=ticker,
                side=ORDER_SIDE,
                action=ORDER_ACTION,
                count=1,
                yes_price=p2_new,
            )
            amend_ms.append((time.perf_counter() - t0) * 1000)
            order_after = resp.get("order", resp)
            if order_after:
                print(
                    f"    Amend2[{n+1}]: id={id2} old_p={p2} new_p={p2_new} "
                    f"status={order_after.get('status')} "
                    f"remaining={order_after.get('remaining_count')} "
                    f"fill={order_after.get('fill_count')}"
                )
                id2 = order_after.get("order_id") or id2
                if order_after.get("status") == "executed":
                    last_error = RuntimeError("Order 2 filled (status=executed)")
                    break
            p2 = p2_new
        except requests.HTTPError as e:
            last_error = e
            break

    print(f"\n  Total successful modifies: {len(amend_ms)}")
    if last_error:
        print(f"  Stopped on error: {last_error}")
        resp = getattr(last_error, "response", None)
        if resp is not None:
            print(f"  Status: {resp.status_code}")
            print(f"  Body: {resp.text[:500]}")

    # Cleanup
    print("\n  Cancelling orders...")
    for oid in (id1, id2):
        try:
            client.cancel_order(oid)
        except requests.HTTPError as e:
            print(f"  [warn] Cancel {oid}: {e}")

    # Timing summary
    print("\n" + "=" * 60)
    print(" TIMING SUMMARY")
    print("=" * 60)
    print(f"  Create (2 orders)         : {t_create_ms:>8.1f} ms")
    if amend_ms:
        print(f"  Amend count              : {len(amend_ms)}")
        print(f"  Amend latency avg        : {sum(amend_ms)/len(amend_ms):>8.1f} ms")
        print(f"  Amend latency min        : {min(amend_ms):>8.1f} ms")
        print(f"  Amend latency max        : {max(amend_ms):>8.1f} ms")
    print("=" * 60)


# ──────────────────────────────────────────────────────────────────────────── #
# Main                                                                         #
# ──────────────────────────────────────────────────────────────────────────── #

def main() -> None:
    # Load .env from the same directory as this script
    _load_dotenv(pathlib.Path(__file__).parent / ".env")

    api_key_id  = os.environ.get("KALSHI_API_KEY_ID", "").strip()
    pem_str     = os.environ.get("KALSHI_PRIVATE_KEY_PEM", "").strip()
    key_path    = os.environ.get("KALSHI_PRIVATE_KEY", "") or os.environ.get("KALSHI_PRIVATE_KEY_PATH", "")
    if not key_path:
        # Fallback: look for a local PEM file next to this script
        default_pem = pathlib.Path(__file__).parent / "kalshi_private.pem"
        if default_pem.is_file():
            key_path = str(default_pem)
    use_demo    = os.environ.get("KALSHI_DEMO", "0") == "1"
    custom_base = os.environ.get("KALSHI_BASE_URL", "")

    if not api_key_id:
        print("Error: KALSHI_API_KEY_ID not found in .env or environment.")
        sys.exit(1)

    if key_path:
        if not os.path.isfile(key_path):
            print(f"Error: private key file not found: {key_path}")
            sys.exit(1)
        private_key = _load_private_key_from_file(key_path)
        print(f"Key source  : file ({key_path})")
    elif pem_str:
        private_key = _load_private_key_from_pem(pem_str)
        print("Key source  : .env (KALSHI_PRIVATE_KEY_PEM)")
    else:
        print(
            "Error: no private key found. Set KALSHI_PRIVATE_KEY_PEM, "
            "KALSHI_PRIVATE_KEY, or KALSHI_PRIVATE_KEY_PATH in .env, "
            "or create kalshi_private.pem next to this script."
        )
        sys.exit(1)

    base_url  = custom_base or (DEMO_BASE if use_demo else PROD_BASE)
    ws_url    = DEMO_WS if use_demo else PROD_WS
    if custom_base:
        # Derive WS URL from a custom base: swap https→wss, /v2→/ws/v2
        ws_url = custom_base.replace("https://", "wss://").replace("/trade-api/v2", "/trade-api/ws/v2")
    env_label = "DEMO" if use_demo else "PRODUCTION"
    print(f"Environment : {env_label}  ({base_url})\n")

    client = KalshiClient(api_key_id, private_key, base_url)

    # ── 1. Find today's NYC weather market ────────────────────────────────── #
    print("=" * 60)
    print(" STEP 1 — Find today's NYC high-temperature market")
    print("=" * 60)

    market = find_todays_nyc_market(client)
    if not market:
        print(f"\nCould not find an open market for ticker {NYC_SERIES}.")
        print("Possible reasons:")
        print("  • The market hasn't opened yet for today.")
        print("  • The ticker date format has changed — check kalshi.com for the current URL.")
        sys.exit(1)

    ticker = market["ticker"]

    # Derive implied YES/NO asks from the live orderbook if the market object
    # does not carry them (newer API versions often omit yes_ask/no_ask).
    try:
        ob_for_asks = client.get_orderbook(ticker)
    except requests.HTTPError:
        ob_for_asks = {}
    else:
        yes_levels = ob_for_asks.get("yes") or []
        no_levels  = ob_for_asks.get("no") or []

        def _best_price(levels: list[list[int]]) -> Optional[int]:
            return levels[-1][0] if levels else None

        best_yes_bid = _best_price(yes_levels)
        best_no_bid  = _best_price(no_levels)

        # Prices are in cents; asks are implied as 100 - best_bid (per docs).
        if best_no_bid is not None:
            market["yes_ask"] = max(0, 100 - best_no_bid)
        if best_yes_bid is not None:
            market["no_ask"] = max(0, 100 - best_yes_bid)

    print_market(market)

    ok, reason = check_guard_rails(client, ticker)
    if not ok:
        print(f"\n  [guard rail] {reason}. Aborting.")
        sys.exit(1)

    # ── 2. WebSocket orderbook snapshot + deltas ──────────────────────────── #
    print("\n" + "=" * 60)
    print(f" STEP 2 — Orderbook via WebSocket for {ticker}")
    print("=" * 60)
    ob_ws, ws_timings = fetch_orderbook_ws(private_key, api_key_id, ws_url, ticker)
    if ob_ws:
        print("  [ws] Received snapshot with sides:",
              f"YES={len(ob_ws.get('yes', []))} levels,",
              f"NO={len(ob_ws.get('no', []))} levels")
        print(f"  Snapshot latency           : {ws_timings.snapshot_ms:7.1f} ms")
        if ws_timings.delta_inter_arrival_ms:
            avg_delta = sum(ws_timings.delta_inter_arrival_ms) / len(ws_timings.delta_inter_arrival_ms)
            print(f"  Avg delta inter-arrival    : {avg_delta:7.1f} ms")
    else:
        print("  [ws] No snapshot received.")

    # ── 3. Batch create + modify loop ─────────────────────────────────────── #
    run_batch_create_and_modify(client, ticker, market)


if __name__ == "__main__":
    main()
