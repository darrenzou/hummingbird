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

1. Resolves a Kalshi market from TARGET_TICKER (full market ticker or event/series ticker).
2. Fetches and prints the orderbook for that market.
3. Places a 1-contract limit buy order at 1¢ YES (well below market so it rests).
4. Immediately cancels that order.

Credentials are loaded from .env (git-ignored). Keys read:
  KALSHI_API_KEY_ID        – API Key ID UUID
  KALSHI_PRIVATE_KEY_PEM   – PEM block of the RSA private key (multi-line OK)
  KALSHI_PRIVATE_KEY_PATH  – Path to a .key file (alternative to PEM block)
  KALSHI_PRIVATE_KEY       – Alias for KALSHI_PRIVATE_KEY_PATH
  KALSHI_BASE_URL          – override base URL (default: production)
  KALSHI_DEMO              – set to "1" to point at demo environment

Usage:
  python nyc_weather_order.py
"""

import base64
import collections
import datetime
import os
import pathlib
import sys
import threading
import time
import uuid
from typing import Any, Dict, List, Optional, Tuple
from urllib.parse import urlparse

import requests
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

# ──────────────────────────────────────────────────────────────────────────── #
# Config                                                                       #
# ──────────────────────────────────────────────────────────────────────────── #

PROD_BASE  = "https://api.elections.kalshi.com/trade-api/v2"
DEMO_BASE  = "https://demo-api.kalshi.co/trade-api/v2"

# Target may be either:
#   • A market ticker  (e.g. KXEUROVISION-26-FIN) — resolved via GET /markets/{ticker}
#   • An event/series ticker (e.g. KXEUROVISION) — first open market from series filter
# (Kalshi website URLs are lowercased; the API uses uppercase tickers.)
TARGET_TICKER = "KXEUROVISION-26-FIN"

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
        if current_key:
            key = current_key.strip()
            # Allow shell-style `export KEY=...` lines by stripping the prefix.
            if key.startswith("export "):
                key = key[len("export ") :].lstrip()
            if key and key not in os.environ:
                os.environ[key] = "\n".join(current_lines)

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
            # look for KEY=value
            if "=" in raw_line:
                key, _, value = raw_line.partition("=")
                current_key = key.rstrip()
                current_lines = [value]  # first line of value (may be empty)
            # lines before any key are ignored
        else:
            # Detect a new KEY=value line vs. a continuation of a multi-line value.
            candidate = raw_line.strip()
            if candidate.startswith("export "):
                candidate = candidate[7:].strip()
            eq_pos = candidate.find("=")
            if eq_pos > 0 and candidate[:eq_pos].replace("_", "").isalnum():
                _flush()
                key, _, value = raw_line.partition("=")
                current_key = key.rstrip()
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
            salt_length=hashes.SHA256().digest_size,  # 32 — matches PSS.DIGEST_LENGTH
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
                 rate_limit: int = 10):
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

    def cancel_order(self, order_id: str) -> Dict:
        return self._delete(f"/portfolio/orders/{order_id}")


# ──────────────────────────────────────────────────────────────────────────── #
# Market lookup                                                                #
# ──────────────────────────────────────────────────────────────────────────── #

def resolve_target_market(client: KalshiClient, ticker: str) -> Optional[Dict]:
    """
    Resolve `ticker` to a single market dict.

    If `ticker` is a full market ticker, use GET /markets/{ticker}.
    Otherwise (or if that returns 404), treat it as an event/series ticker and
    take the first open market from GET /markets?series_ticker=...
    """
    ticker = (ticker or "").strip()
    if not ticker:
        return None

    try:
        market = client.get_market(ticker)
        if market and market.get("ticker"):
            print(f"  Using market ticker: {ticker}")
            return market
    except requests.HTTPError as e:
        status = e.response.status_code if e.response is not None else None
        if status != 404:
            raise
        print(f"  No market named {ticker!r} (404) — trying as event/series ticker")

    print(f"  Series search: series_ticker={ticker}")
    markets = client.get_markets_by_series(ticker)
    if not markets:
        return None
    market = markets[0]
    print(f"  Found: {market.get('ticker')}  ({len(markets)} open market(s) in series)")
    return market


# ──────────────────────────────────────────────────────────────────────────── #
# Display helpers                                                              #
# ──────────────────────────────────────────────────────────────────────────── #

def _parse_dollars_to_cents(v: Any) -> Optional[int]:
    """Kalshi *_dollars fields are fixed-point strings, e.g. '0.4200' → 42¢."""
    if v is None or v == "":
        return None
    if isinstance(v, bool):
        return None
    if isinstance(v, int):
        return v
    try:
        return int(round(float(v) * 100))
    except (TypeError, ValueError):
        return None


def _parse_count_fp(v: Any) -> Optional[int]:
    """Parse contract counts: int legacy or fp string '10.00' → 10."""
    if v is None or v == "":
        return None
    if isinstance(v, bool):
        return None
    if isinstance(v, int):
        return v
    try:
        return int(float(v))
    except (TypeError, ValueError):
        return None


def _fmt_cents(label: str, cents: Optional[int]) -> None:
    if cents is None:
        print(f"  {label:<11}: —")
    else:
        print(f"  {label:<11}: {cents}¢")


def normalize_market_quote_cents(m: Dict) -> None:
    """Fill yes_ask / no_ask (integer ¢) from v2 *_dollars strings when present."""
    if m.get("yes_ask") is None:
        c = _parse_dollars_to_cents(m.get("yes_ask_dollars"))
        if c is not None:
            m["yes_ask"] = c
    if m.get("no_ask") is None:
        c = _parse_dollars_to_cents(m.get("no_ask_dollars"))
        if c is not None:
            m["no_ask"] = c


def enrich_market_asks_from_orderbook(market: Dict, ob: Dict) -> None:
    """
    If yes_ask/no_ask still missing, derive from REST orderbook (bids only).
    Implied ask = 100¢ − best opposing bid.
    """
    if market.get("yes_ask") is not None and market.get("no_ask") is not None:
        return
    yes_levels = ob.get("yes") or []
    no_levels = ob.get("no") or []

    def _best_bid(levels: List) -> Optional[int]:
        return levels[-1][0] if levels else None

    best_yes_bid = _best_bid(yes_levels)
    best_no_bid = _best_bid(no_levels)

    if market.get("yes_ask") is None and best_no_bid is not None:
        market["yes_ask"] = max(0, 100 - best_no_bid)
    if market.get("no_ask") is None and best_yes_bid is not None:
        market["no_ask"] = max(0, 100 - best_yes_bid)


def _quote_cents_from_market(m: Dict, int_key: str, dollars_key: str) -> Optional[int]:
    v = m.get(int_key)
    if v is not None:
        try:
            return int(v)
        except (TypeError, ValueError):
            pass
    return _parse_dollars_to_cents(m.get(dollars_key))


def print_market(m: Dict) -> None:
    print("  Ticker     :", m.get("ticker"))
    print("  Title      :", m.get("title", m.get("question", "—")))
    _fmt_cents("Yes ask", _quote_cents_from_market(m, "yes_ask", "yes_ask_dollars"))
    _fmt_cents("No ask", _quote_cents_from_market(m, "no_ask", "no_ask_dollars"))
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


def _order_limit_cents(o: Dict, int_key: str, dollars_key: str) -> Optional[int]:
    v = o.get(int_key)
    if v is not None:
        try:
            return int(v)
        except (TypeError, ValueError):
            pass
    return _parse_dollars_to_cents(o.get(dollars_key))


def _order_contract_counts(o: Dict) -> Tuple[Optional[int], Optional[int], Optional[int]]:
    initial = _parse_count_fp(o.get("initial_count_fp"))
    if initial is None:
        initial = _parse_count_fp(o.get("initial_count"))
    remaining = _parse_count_fp(o.get("remaining_count_fp"))
    if remaining is None:
        remaining = _parse_count_fp(o.get("remaining_count"))
    filled = _parse_count_fp(o.get("fill_count_fp"))
    if filled is None:
        filled = _parse_count_fp(o.get("fill_count"))
    return initial, remaining, filled


def _fmt_contracts(label: str, n: Optional[int]) -> None:
    if n is None:
        print(f"  {label:<11}: —")
    else:
        print(f"  {label:<11}: {n}")


def print_order(order: Dict) -> None:
    oid = order.get("order_id") or order.get("id")
    print(f"  Order ID   : {oid}")
    print(f"  Status     : {order.get('status')}")
    print(f"  Side       : {order.get('side')}  {order.get('action')}")
    _fmt_cents("Yes price", _order_limit_cents(order, "yes_price", "yes_price_dollars"))
    _fmt_cents("No price", _order_limit_cents(order, "no_price", "no_price_dollars"))
    ini, rem, fil = _order_contract_counts(order)
    _fmt_contracts("Initial", ini)
    _fmt_contracts("Remaining", rem)
    _fmt_contracts("Filled", fil)


# ──────────────────────────────────────────────────────────────────────────── #
# Main                                                                         #
# ──────────────────────────────────────────────────────────────────────────── #

def main() -> None:
    # Load .env from the same directory as this script
    _load_dotenv(pathlib.Path(__file__).parent / ".env")

    api_key_id  = os.environ.get("KALSHI_API_KEY_ID", "").strip()
    pem_str     = os.environ.get("KALSHI_PRIVATE_KEY_PEM", "").strip()
    key_path    = os.environ.get("KALSHI_PRIVATE_KEY_PATH", "") or os.environ.get("KALSHI_PRIVATE_KEY", "")
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
            "KALSHI_PRIVATE_KEY_PATH, or KALSHI_PRIVATE_KEY in .env, "
            "or create kalshi_private.pem next to this script."
        )
        sys.exit(1)

    base_url  = custom_base or (DEMO_BASE if use_demo else PROD_BASE)
    env_label = "DEMO" if use_demo else "PRODUCTION"
    print(f"Environment : {env_label}  ({base_url})\n")

    client = KalshiClient(api_key_id, private_key, base_url)

    # ── 1. Resolve market from TARGET_TICKER (market or event ticker) ─────── #
    print("=" * 60)
    print(" STEP 1 — Resolve market (market ticker or event/series ticker)")
    print("=" * 60)

    market = resolve_target_market(client, TARGET_TICKER)
    if not market:
        print(f"\nCould not resolve an open market from {TARGET_TICKER!r}.")
        print("Possible reasons:")
        print("  • Wrong or mistyped market/event ticker.")
        print("  • No open markets for that series yet — check kalshi.com.")
        sys.exit(1)

    ticker = market["ticker"]

    t0 = time.perf_counter()
    try:
        ob = client.get_orderbook(ticker)
    except requests.HTTPError:
        ob = {}
    t_orderbook = time.perf_counter() - t0

    normalize_market_quote_cents(market)
    enrich_market_asks_from_orderbook(market, ob)
    print_market(market)

    # ── 2. Orderbook detail ─────────────────────────────────────────────────── #
    print("\n" + "=" * 60)
    print(f" STEP 2 — Orderbook for {ticker}")
    print("=" * 60)

    print_orderbook(ob)
    print(f"  Latency: {t_orderbook * 1000:.1f} ms")

    # ── 3a. Place order ───────────────────────────────────────────────────── #
    print("\n" + "=" * 60)
    print(f" STEP 3a — Placing {ORDER_COUNT}-contract limit buy at {ORDER_YES_PRICE}¢ YES")
    print("=" * 60)

    try:
        t0 = time.perf_counter()
        place_resp = client.create_order(
            ticker    = ticker,
            side      = ORDER_SIDE,
            action    = ORDER_ACTION,
            count     = ORDER_COUNT,
            yes_price = ORDER_YES_PRICE,
        )
        t_place = time.perf_counter() - t0
    except requests.HTTPError as e:
        print(f"  [error] Could not place order: {e}")
        print(f"  Response: {e.response.text if e.response else 'no body'}")
        sys.exit(1)

    order = place_resp.get("order", place_resp)
    print_order(order)
    print(f"  Latency: {t_place * 1000:.1f} ms")
    order_id = order.get("order_id") or order.get("id")

    if not order_id:
        print("  [error] No order_id returned. Cannot cancel.")
        sys.exit(1)

    # ── 3b. Cancel order ──────────────────────────────────────────────────── #
    print("\n" + "=" * 60)
    print(f" STEP 3b — Cancelling order {order_id}")
    print("=" * 60)

    try:
        t0 = time.perf_counter()
        cancel_resp = client.cancel_order(order_id)
        t_cancel = time.perf_counter() - t0
    except requests.HTTPError as e:
        print(f"  [error] Could not cancel order: {e}")
        print(f"  Response: {e.response.text if e.response else 'no body'}")
        sys.exit(1)

    cancelled_order = cancel_resp.get("order", cancel_resp)
    reduced_raw     = cancel_resp.get("reduced_by")
    reduced_n       = _parse_count_fp(reduced_raw)
    print_order(cancelled_order)
    print(f"  Latency: {t_cancel * 1000:.1f} ms")
    if reduced_n is not None:
        print(f"  Reduced by : {reduced_n} contract(s)")
    elif reduced_raw is not None:
        print(f"  Reduced by : {reduced_raw}")
    else:
        print("  Reduced by : —")

    print("\n" + "=" * 60)
    print(" TIMING SUMMARY")
    print("=" * 60)
    print(f"  Orderbook fetch : {t_orderbook * 1000:>8.1f} ms")
    print(f"  Place order     : {t_place * 1000:>8.1f} ms")
    print(f"  Cancel order    : {t_cancel * 1000:>8.1f} ms")
    print(f"  {'─' * 28}")
    print(f"  Total           : {(t_orderbook + t_place + t_cancel) * 1000:>8.1f} ms")
    print(f"\nDone. Order {order_id} placed and cancelled successfully.")


if __name__ == "__main__":
    main()
