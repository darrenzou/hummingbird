"""
market_lookup.py

Reads config.json (array or single-object) and queries both Kalshi and
Polymarket APIs for each configured market pair, printing a summary of
current market data (title, best bid/ask, volumes, status).

Credentials are loaded from .env:
  Kalshi:    KALSHI_API_KEY_ID, KALSHI_PRIVATE_KEY_PATH or KALSHI_PRIVATE_KEY_PEM
  Polymarket: no credentials required for read-only CLOB orderbook queries

Usage:
  python market_lookup.py [config.json]
"""

import base64
import datetime
import json
import os
import pathlib
import sys
from typing import Any, Dict, List, Optional
from urllib.parse import urlparse

import requests
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

# ── Endpoints ─────────────────────────────────────────────────────────────── #

KALSHI_BASE   = "https://api.elections.kalshi.com/trade-api/v2"
CLOB_BASE     = "https://clob.polymarket.com"
GAMMA_BASE    = "https://gamma-api.polymarket.com"


# ── .env loader ───────────────────────────────────────────────────────────── #

def _load_dotenv(env_path: pathlib.Path) -> None:
    """
    Parse a .env file and inject values into os.environ.
    Existing environment variables take precedence.
    Supports multi-line values (continuation until blank line).
    """
    if not env_path.exists():
        return

    current_key: Optional[str] = None
    current_lines: List[str] = []

    def _flush() -> None:
        if current_key and current_key not in os.environ:
            os.environ[current_key] = "\n".join(current_lines)

    for raw in env_path.read_text(encoding="utf-8").splitlines():
        if raw.strip() == "":
            if current_key:
                _flush()
                current_key = None
                current_lines = []
            continue
        if raw.lstrip().startswith("#"):
            continue
        if current_key is None:
            if "=" in raw:
                key, _, value = raw.partition("=")
                current_key   = key.strip()
                current_lines = [value]
        else:
            current_lines.append(raw)
    _flush()


# ── Kalshi auth ────────────────────────────────────────────────────────────── #

def _load_kalshi_key() -> Optional[Any]:
    """Load the RSA private key from env (PEM block or file path)."""
    pem   = os.environ.get("KALSHI_PRIVATE_KEY_PEM", "")
    fpath = (os.environ.get("KALSHI_PRIVATE_KEY_PATH", "")
             or os.environ.get("KALSHI_PRIVATE_KEY", ""))
    try:
        if fpath and os.path.isfile(fpath):
            with open(fpath, "rb") as f:
                return serialization.load_pem_private_key(
                    f.read(), password=None, backend=default_backend())
        if pem:
            return serialization.load_pem_private_key(
                pem.encode(), password=None, backend=default_backend())
    except Exception as exc:
        print(f"  [kalshi] Could not load private key: {exc}", file=sys.stderr)
    return None


def _kalshi_auth_headers(private_key, api_key_id: str,
                          method: str, path: str) -> Dict[str, str]:
    ts  = str(int(datetime.datetime.now().timestamp() * 1000))
    msg = f"{ts}{method}{path.split('?')[0]}".encode()
    sig = private_key.sign(
        msg,
        padding.PSS(mgf=padding.MGF1(hashes.SHA256()),
                    salt_length=hashes.SHA256().digest_size),
        hashes.SHA256(),
    )
    return {
        "KALSHI-ACCESS-KEY":       api_key_id,
        "KALSHI-ACCESS-TIMESTAMP": ts,
        "KALSHI-ACCESS-SIGNATURE": base64.b64encode(sig).decode(),
        "Content-Type":            "application/json",
        "Accept":                  "application/json",
    }


# ── Config loader ─────────────────────────────────────────────────────────── #

def load_config(path: str) -> List[Dict]:
    """
    Load config.json and return a list of pair dicts.
    Accepts a JSON array or a single JSON object.
    """
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    if isinstance(data, dict):
        data = [data]
    if not isinstance(data, list) or not data:
        raise ValueError("config must be a JSON array or object")
    for item in data:
        if "polymarket_token_id" not in item or "kalshi_ticker" not in item:
            raise ValueError(f"pair missing required fields: {item}")
    return data


# ── Kalshi market fetch ───────────────────────────────────────────────────── #

def fetch_kalshi_market(session: requests.Session,
                         ticker: str,
                         api_key_id: str,
                         private_key) -> Optional[Dict]:
    """Fetch single market info from Kalshi REST API."""
    path      = f"/markets/{ticker}"
    api_pfx   = urlparse(KALSHI_BASE).path
    sign_path = api_pfx + path
    url       = KALSHI_BASE + path

    try:
        hdrs = _kalshi_auth_headers(private_key, api_key_id, "GET", sign_path)
        resp = session.get(url, headers=hdrs, timeout=10)
        resp.raise_for_status()
        data = resp.json()
        return data.get("market", data)
    except requests.HTTPError as exc:
        print(f"  [kalshi] HTTP error for {ticker}: {exc.response.status_code} "
              f"{exc.response.text[:120]}", file=sys.stderr)
    except Exception as exc:
        print(f"  [kalshi] Error for {ticker}: {exc}", file=sys.stderr)
    return None


def fetch_kalshi_orderbook(session: requests.Session, ticker: str) -> Optional[Dict]:
    """Fetch orderbook for a Kalshi market (no auth required)."""
    url = f"{KALSHI_BASE}/markets/{ticker}/orderbook"
    try:
        resp = session.get(url, params={"depth": 0},
                           headers={"Accept": "application/json"}, timeout=10)
        resp.raise_for_status()
        data = resp.json()
        return data.get("orderbook", data)
    except Exception as exc:
        print(f"  [kalshi] Orderbook error for {ticker}: {exc}", file=sys.stderr)
    return None


# ── Polymarket market fetch ───────────────────────────────────────────────── #

def fetch_poly_orderbook(session: requests.Session, token_id: str) -> Optional[Dict]:
    """Fetch CLOB orderbook for a Polymarket token (no auth required)."""
    try:
        resp = session.get(f"{CLOB_BASE}/book",
                           params={"token_id": token_id}, timeout=10)
        resp.raise_for_status()
        data = resp.json()
        if "error" in data:
            print(f"  [poly] CLOB error for token {token_id[:20]}…: {data['error']}",
                  file=sys.stderr)
            return None
        return data
    except Exception as exc:
        print(f"  [poly] Orderbook error for token {token_id[:20]}…: {exc}",
              file=sys.stderr)
    return None


def fetch_poly_market_meta(session: requests.Session, token_id: str) -> Optional[Dict]:
    """Look up Gamma API market metadata by clobTokenId."""
    try:
        resp = session.get(
            f"{GAMMA_BASE}/markets",
            params={"clob_token_ids": token_id, "limit": 1},
            timeout=10,
        )
        resp.raise_for_status()
        data = resp.json()
        markets = data if isinstance(data, list) else data.get("markets", [])
        if markets:
            return markets[0]
    except Exception:
        pass

    # Fallback: search by token ID as keyword
    try:
        resp = session.get(
            f"{GAMMA_BASE}/markets",
            params={"token_id": token_id, "limit": 1},
            timeout=10,
        )
        resp.raise_for_status()
        data = resp.json()
        markets = data if isinstance(data, list) else data.get("markets", [])
        if markets:
            return markets[0]
    except Exception:
        pass

    return None


# ── Display helpers ───────────────────────────────────────────────────────── #

def _best_bid_ask_poly(ob: Dict):
    """Return (best_bid_price, best_bid_size, best_ask_price, best_ask_size) from Poly CLOB book."""
    bids = sorted(ob.get("bids", []), key=lambda x: float(x.get("price", 0)), reverse=True)
    asks = sorted(ob.get("asks", []), key=lambda x: float(x.get("price", 0)))
    bb_p = float(bids[0]["price"]) if bids else None
    bb_s = float(bids[0]["size"])  if bids else None
    ba_p = float(asks[0]["price"]) if asks else None
    ba_s = float(asks[0]["size"])  if asks else None
    return bb_p, bb_s, ba_p, ba_s


def _best_bid_ask_kalshi(ob: Dict):
    """Return (best_yes_bid_price, best_yes_bid_qty, best_yes_ask_price, best_yes_ask_qty).

    Kalshi orderbooks expose two bid sides (YES bids and NO bids); there are no
    separate ask arrays.  The best YES ask is derived from the NO bid side:
      best_YES_ask = 100 - max(NO_bids.price)
    The depth=0 orderbook includes every resting order, so NO bids can include
    far-out-of-the-money levels (e.g. 1¢).  We must take the HIGHEST NO bid
    (yes_asks[-1] after ascending sort) not the lowest (yes_asks[0]).
    """
    yes_bids = sorted(ob.get("yes", []), key=lambda x: x[0])   # asc → [-1] = best bid
    no_bids  = sorted(ob.get("no",  []), key=lambda x: x[0])   # asc → [-1] = best (highest) NO bid
    bb_p = yes_bids[-1][0] if yes_bids else None
    bb_q = yes_bids[-1][1] if yes_bids else None
    ba_p = (100 - no_bids[-1][0]) if no_bids else None          # 100 - highest NO bid
    ba_q = no_bids[-1][1]          if no_bids else None
    return bb_p, bb_q, ba_p, ba_q


def print_pair_summary(idx: int, pair: Dict,
                        k_market: Optional[Dict], k_ob: Optional[Dict],
                        p_meta: Optional[Dict],   p_ob: Optional[Dict]) -> None:
    sep = "─" * 64
    print(f"\n{sep}")
    print(f" Pair {idx}  │  Kalshi: {pair['kalshi_ticker']}")
    print(f"        │  Poly token: {pair['polymarket_token_id'][:24]}…")
    print(sep)

    # ── Kalshi ──────────────────────────────────────────────────────────────
    print("\n  ┌─ KALSHI")
    if k_market:
        title         = k_market.get("title", k_market.get("subtitle", "—"))
        status        = k_market.get("status", "—")
        yes_ask_c     = k_market.get("yes_ask")
        no_ask_c      = k_market.get("no_ask")
        yes_bid_c     = k_market.get("yes_bid")
        close_time    = k_market.get("close_time", k_market.get("expiration_time", "—"))
        volume        = k_market.get("volume")
        volume_24h    = k_market.get("volume_24h")
        open_interest = k_market.get("open_interest")
        print(f"  │  Title        : {title}")
        print(f"  │  Status       : {status}")
        print(f"  │  Yes ask      : {yes_ask_c}¢   No ask: {no_ask_c}¢")
        print(f"  │  Yes bid      : {yes_bid_c}¢")
        print(f"  │  Closes       : {close_time}")
        print(f"  │  Volume       : {volume:,} contracts" if volume is not None else "  │  Volume       : —")
        print(f"  │  Volume (24h) : {volume_24h:,} contracts" if volume_24h is not None else "  │  Volume (24h) : —")
        print(f"  │  Open interest: {open_interest:,} contracts" if open_interest is not None else "  │  Open interest: —")
    else:
        print(f"  │  (market info unavailable)")

    if k_ob:
        bb_p, bb_q, ba_p, ba_q = _best_bid_ask_kalshi(k_ob)
        print(f"  │  Orderbook →  YES bid: {bb_p}¢ × {bb_q}  │  YES ask: {ba_p}¢ × {ba_q}")
    else:
        print(f"  │  Orderbook unavailable")

    # ── Polymarket ──────────────────────────────────────────────────────────
    print("\n  ┌─ POLYMARKET")
    if p_meta:
        question   = p_meta.get("question", p_meta.get("title", "—"))
        neg_risk   = p_meta.get("negRisk", pair.get("neg_risk", False))
        end_date   = p_meta.get("endDate", p_meta.get("close_time", "—"))
        try:
            prices_raw = p_meta.get("outcomePrices", "")
            if isinstance(prices_raw, str):
                prices = json.loads(prices_raw)
            else:
                prices = prices_raw or []
            price_yes = f"${float(prices[0]):.2f}" if prices else "—"
        except Exception:
            price_yes = "—"
        try:
            vol_raw = p_meta.get("volume") or p_meta.get("volumeNum")
            volume_usd = f"${float(vol_raw):,.2f}" if vol_raw is not None else "—"
        except Exception:
            volume_usd = "—"
        try:
            vol24_raw = p_meta.get("volume24hr")
            volume_24h_usd = f"${float(vol24_raw):,.2f}" if vol24_raw is not None else "—"
        except Exception:
            volume_24h_usd = "—"
        print(f"  │  Question     : {question}")
        print(f"  │  NegRisk      : {neg_risk}")
        print(f"  │  YES price    : {price_yes}")
        print(f"  │  Ends         : {end_date}")
        print(f"  │  Volume       : {volume_usd}")
        print(f"  │  Volume (24h) : {volume_24h_usd}")
    else:
        print(f"  │  (market metadata unavailable — token may not be in Gamma index)")

    if p_ob:
        bb_p, bb_s, ba_p, ba_s = _best_bid_ask_poly(p_ob)
        bb_cents = f"{bb_p * 100:.1f}¢" if bb_p is not None else "—"
        ba_cents = f"{ba_p * 100:.1f}¢" if ba_p is not None else "—"
        print(f"  │  Orderbook →  bid: {bb_cents} × {bb_s}  │  ask: {ba_cents} × {ba_s}")
    else:
        print(f"  │  Orderbook unavailable")

    # ── Arbitrage snapshot ──────────────────────────────────────────────────
    if k_ob and p_ob:
        k_bb, _, k_ba, _ = _best_bid_ask_kalshi(k_ob)
        p_bb, _, p_ba, _ = _best_bid_ask_poly(p_ob)
        if None not in (k_bb, k_ba, p_bb, p_ba):
            p_bb_c = p_bb * 100
            p_ba_c = p_ba * 100
            arb_ok = p_bb_c > k_bb and p_ba_c < k_ba
            spread_bid = p_bb_c - k_bb
            spread_ask = k_ba  - p_ba_c
            print(f"\n  ┌─ ARB SNAPSHOT")
            print(f"  │  Poly bid {p_bb_c:.1f}¢ vs Kalshi bid {k_bb}¢  →  Δ = {spread_bid:+.1f}¢")
            print(f"  │  Poly ask {p_ba_c:.1f}¢ vs Kalshi ask {k_ba}¢  →  Δ = {spread_ask:+.1f}¢")
            flag = "✓  ARB OPPORTUNITY" if arb_ok else "✗  no arb"
            print(f"  │  {flag}  (need poly_bid > kalshi_bid AND poly_ask < kalshi_ask)")


# ── Main ──────────────────────────────────────────────────────────────────── #

def main() -> None:
    config_path = sys.argv[1] if len(sys.argv) > 1 else "config.json"
    _load_dotenv(pathlib.Path(__file__).parent / ".env")

    api_key_id  = os.environ.get("KALSHI_API_KEY_ID", "")
    private_key = _load_kalshi_key()

    if not api_key_id or private_key is None:
        print("Warning: Kalshi credentials missing — market detail will be limited.")
        print("Set KALSHI_API_KEY_ID and KALSHI_PRIVATE_KEY_PATH (or KALSHI_PRIVATE_KEY_PEM) in .env\n")

    try:
        pairs = load_config(config_path)
    except (FileNotFoundError, ValueError, json.JSONDecodeError) as exc:
        print(f"Error loading {config_path}: {exc}", file=sys.stderr)
        sys.exit(1)

    print(f"Loaded {len(pairs)} market pair(s) from {config_path}")

    session = requests.Session()
    session.headers.update({"Accept": "application/json"})

    for idx, pair in enumerate(pairs):
        ticker   = pair["kalshi_ticker"]
        token_id = pair["polymarket_token_id"]

        print(f"\n[{idx}] Querying {ticker} / {token_id[:20]}…", flush=True)

        # Kalshi
        k_market = (fetch_kalshi_market(session, ticker, api_key_id, private_key)
                    if api_key_id and private_key else None)
        k_ob     = fetch_kalshi_orderbook(session, ticker)

        # Polymarket
        p_meta = fetch_poly_market_meta(session, token_id)
        p_ob   = fetch_poly_orderbook(session, token_id)

        print_pair_summary(idx, pair, k_market, k_ob, p_meta, p_ob)

    print(f"\n{'─' * 64}")
    print(f"Done — {len(pairs)} pair(s) checked.")


if __name__ == "__main__":
    main()
