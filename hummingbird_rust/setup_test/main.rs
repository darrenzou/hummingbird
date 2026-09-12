//! Historical connectivity harness (Kalshi + Polymarket). Not maintained.
//!
//! - Finds the first market on each venue with best bid > 0.15.
//! - Polymarket: **GTC** buy using Gamma **`orderMinSize`** + **`orderPriceMinTickSize`** (then cancel/replace).
//!
//! Run from repo root: `cargo run -p hummingbird_rust --bin setup_connectivity_test`
//!
//! Polymarket **400 Invalid order payload** is almost always **EIP-712 / `signatureType` / tick / negRisk**,
//! not API-key HMAC (**401/403**). Use `POLY_SIGNATURE_TYPE`: `0` when `ETH_PRIV_KEY` owns `POLY_ADDRESS` (EOA),
//! `1` for Polymarket proxy-style funder if that is your setup.

use std::path::Path;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use hummingbird_rust::arb_config::{load_dotenv_override, ArbCreds};
use hummingbird_rust::kalshi_live::{KalshiBatchOrder, KalshiLive};
use hummingbird_rust::poly_live::{
    fetch_clob_book_public, floor_price_to_tick, format_poly_price_for_tick, PolyClobConstraints,
    PolyLive,
};

const MIN_BID: f64 = 0.15;
const MAX_THROTTLE_ATTEMPTS: u32 = 8;
const KALSHI_REST: &str = "https://api.elections.kalshi.com/trade-api/v2";
const GAMMA_MARKETS: &str = "https://gamma-api.polymarket.com/markets";

#[derive(Debug)]
enum TestErr {
    Auth { venue: &'static str, op: String },
    Throttle { venue: &'static str, op: String },
    Other(String),
}

impl std::fmt::Display for TestErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestErr::Auth { venue, op } => write!(f, "authentication error ({venue}) during {op}"),
            TestErr::Throttle { venue, op } => {
                write!(f, "throttle limit reached ({venue}) during {op}")
            }
            TestErr::Other(s) => write!(f, "{s}"),
        }
    }
}

fn dotenv_path() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"))
}

fn kalshi_best_yes_bid_dollars(ticker: &str) -> anyhow::Result<f64> {
    let (bids, _) = KalshiLive::fetch_public_orderbook(ticker)?;
    let best: f64 = bids.iter().map(|(c, _)| *c / 100.0).fold(0.0_f64, f64::max);
    Ok(best)
}

#[derive(Deserialize)]
struct KalshiMarketsPage {
    markets: Option<Vec<Value>>,
    #[serde(default)]
    cursor: Option<String>,
}

fn discover_kalshi_ticker() -> anyhow::Result<String> {
    let mut cursor: Option<String> = None;
    for _page in 0..10 {
        let mut url = format!("{KALSHI_REST}/markets?limit=200&status=open");
        if let Some(c) = &cursor {
            url.push_str("&cursor=");
            url.push_str(c);
        }
        let resp = ureq::get(&url)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| anyhow::anyhow!("Kalshi list markets: {e}"))?;
        let status = resp.status();
        let body = resp.into_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            anyhow::bail!(
                "Kalshi /markets HTTP {status}: {}",
                &body[..body.len().min(400)]
            );
        }
        let page: KalshiMarketsPage =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Kalshi markets JSON: {e}"))?;
        let Some(markets) = page.markets else {
            anyhow::bail!("Kalshi /markets: missing markets array");
        };
        for m in &markets {
            let ticker = m.get("ticker").and_then(|v| v.as_str()).unwrap_or_default();
            if ticker.is_empty() {
                continue;
            }
            let best = match kalshi_best_yes_bid_dollars(ticker) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if best > MIN_BID {
                eprintln!(
                    "[kalshi] picked {ticker} (best YES bid ≈ {:.3} USD > {MIN_BID})",
                    best
                );
                return Ok(ticker.to_string());
            }
        }
        cursor = page.cursor.filter(|c| !c.is_empty());
        if cursor.is_none() {
            break;
        }
    }
    anyhow::bail!("no Kalshi market found with best YES bid > {MIN_BID}");
}

fn gamma_clob_tokens(m: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = m.get("clobTokenIds").and_then(|v| v.as_array()) {
        for t in arr {
            if let Some(s) = t.as_str() {
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        }
    } else if let Some(s) = m.get("clobTokenIds").and_then(|v| v.as_str()) {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(s) {
            out.extend(v.into_iter().filter(|t| !t.is_empty()));
        }
    }
    out
}

/// Single outcome token plus CLOB constraints from the parent Gamma market row.
#[derive(Debug, Clone)]
struct PolyPick {
    token_id: String,
    neg_risk: bool,
    /// Contracts per order (at least Gamma `orderMinSize`, rounded up).
    base_size: u64,
    tick: f64,
    /// Lowest grid price (one tick) — far below typical ≥0.15 best bids in discovery.
    bid_price_low: f64,
    /// Second grid price (`bid_price_low + tick`, capped) for price-amend cycles.
    bid_price_high: f64,
}

fn gamma_json_f64(m: &Value, key: &str, default: f64) -> f64 {
    m.get(key)
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str()?.parse().ok())
                .or_else(|| v.as_i64().map(|i| i as f64))
        })
        .filter(|x| x.is_finite())
        .unwrap_or(default)
}

fn gamma_neg_risk(m: &Value) -> bool {
    m.get("negRisk")
        .and_then(|v| v.as_bool())
        .or_else(|| m.get("neg_risk").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

fn poly_pick_from_gamma(tid: String, m: &Value) -> PolyPick {
    let neg_risk = gamma_neg_risk(m);
    let mut tick = gamma_json_f64(m, "orderPriceMinTickSize", 0.01);
    if tick <= 0.0 || !tick.is_finite() {
        tick = 0.01;
    }
    let min_raw = gamma_json_f64(m, "orderMinSize", 1.0);
    let base_size = min_raw.max(1.0).ceil().max(1.0) as u64;

    // Deepest cheap bid: one tick (e.g. 0.01), aligned.
    let bid_price_low = floor_price_to_tick(tick, tick);
    let mut bid_price_high = floor_price_to_tick(bid_price_low + tick, tick);
    if bid_price_high <= bid_price_low {
        bid_price_high = floor_price_to_tick(bid_price_low + tick * 2.0, tick);
    }
    if bid_price_high > 0.99 {
        bid_price_high = floor_price_to_tick((bid_price_low + tick).min(0.99), tick);
    }
    if bid_price_high <= bid_price_low {
        bid_price_high = floor_price_to_tick((bid_price_low + tick).min(0.99), tick);
    }

    eprintln!(
        "[poly] Gamma constraints: tick={} orderMinSize(raw)={min_raw} → base_size={base_size} | bid prices {} / {} (GTC)",
        format_poly_price_for_tick(tick, tick),
        format_poly_price_for_tick(bid_price_low, tick),
        format_poly_price_for_tick(bid_price_high, tick)
    );

    PolyPick {
        token_id: tid,
        neg_risk,
        base_size,
        tick,
        bid_price_low,
        bid_price_high,
    }
}

fn discover_poly_market() -> anyhow::Result<PolyPick> {
    for offset in [0u32, 100, 200, 300] {
        let url = format!("{GAMMA_MARKETS}?limit=100&active=true&closed=false&offset={offset}");
        let resp = ureq::get(&url)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| anyhow::anyhow!("Gamma markets: {e}"))?;
        let status = resp.status();
        let body = resp.into_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            anyhow::bail!(
                "Gamma /markets HTTP {status}: {}",
                &body[..body.len().min(400)]
            );
        }
        let arr: Vec<Value> =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Gamma JSON: {e}"))?;
        for m in &arr {
            for tid in gamma_clob_tokens(m) {
                let book = match fetch_clob_book_public(&tid) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let best = book.bids.first().map(|l| l.price).unwrap_or(0.0);
                if best > MIN_BID {
                    let pick = poly_pick_from_gamma(tid, m);
                    eprintln!(
                        "[poly] picked token {}… (best bid {:.3} > {MIN_BID})",
                        &pick.token_id[..pick.token_id.len().min(12)],
                        best
                    );
                    return Ok(pick);
                }
            }
        }
    }
    anyhow::bail!("no Polymarket token found with best bid > {MIN_BID}");
}

fn backoff_ms(attempt: u32) -> u64 {
    500u64.saturating_mul(attempt.max(1) as u64)
}

/// Kalshi-style `(status, body)` without network flag.
fn retry_pair(
    venue: &'static str,
    op: &str,
    mut f: impl FnMut() -> (u16, String),
) -> Result<(u16, String), TestErr> {
    for attempt in 1..=MAX_THROTTLE_ATTEMPTS {
        let (status, body) = f();
        if status == 401 || status == 403 {
            eprintln!(
                "authentication error ({venue}) during {op}: HTTP {status} {}",
                body.chars().take(500).collect::<String>()
            );
            return Err(TestErr::Auth {
                venue,
                op: op.to_string(),
            });
        }
        if status == 429 {
            eprintln!(
                "rate limit throttle ({venue}): {op} — attempt {attempt}/{MAX_THROTTLE_ATTEMPTS}"
            );
            if attempt == MAX_THROTTLE_ATTEMPTS {
                eprintln!("throttle limit reached");
                return Err(TestErr::Throttle {
                    venue,
                    op: op.to_string(),
                });
            }
            thread::sleep(Duration::from_millis(backoff_ms(attempt)));
            continue;
        }
        return Ok((status, body));
    }
    eprintln!("throttle limit reached");
    Err(TestErr::Throttle {
        venue,
        op: op.to_string(),
    })
}

fn retry_poly(
    venue: &'static str,
    op: &str,
    mut f: impl FnMut() -> (u16, String, bool),
) -> Result<(u16, String), TestErr> {
    for attempt in 1..=MAX_THROTTLE_ATTEMPTS {
        let (status, body, ok) = f();
        if !ok {
            return Err(TestErr::Other(format!(
                "network error ({venue}) during {op}: connection failed"
            )));
        }
        if status == 401 || status == 403 {
            eprintln!(
                "authentication error ({venue}) during {op}: HTTP {status} {}",
                body.chars().take(500).collect::<String>()
            );
            return Err(TestErr::Auth {
                venue,
                op: op.to_string(),
            });
        }
        if status == 429 {
            eprintln!(
                "rate limit throttle ({venue}): {op} — attempt {attempt}/{MAX_THROTTLE_ATTEMPTS}"
            );
            if attempt == MAX_THROTTLE_ATTEMPTS {
                eprintln!("throttle limit reached");
                return Err(TestErr::Throttle {
                    venue,
                    op: op.to_string(),
                });
            }
            thread::sleep(Duration::from_millis(backoff_ms(attempt)));
            continue;
        }
        return Ok((status, body));
    }
    eprintln!("throttle limit reached");
    Err(TestErr::Throttle {
        venue,
        op: op.to_string(),
    })
}

fn retry_kalshi_batch(
    k: &KalshiLive,
    orders: &[KalshiBatchOrder],
    op: &str,
) -> Result<(Vec<String>, u16), TestErr> {
    for attempt in 1..=MAX_THROTTLE_ATTEMPTS {
        let (ids, status) = k
            .batch_place_orders(orders)
            .map_err(|e| TestErr::Other(format!("kalshi {op}: {e}")))?;
        if status == 401 || status == 403 {
            eprintln!("authentication error (kalshi) during {op}: HTTP {status}",);
            return Err(TestErr::Auth {
                venue: "kalshi",
                op: op.to_string(),
            });
        }
        if status == 429 {
            eprintln!(
                "rate limit throttle (kalshi): {op} — attempt {attempt}/{MAX_THROTTLE_ATTEMPTS}"
            );
            if attempt == MAX_THROTTLE_ATTEMPTS {
                eprintln!("throttle limit reached");
                return Err(TestErr::Throttle {
                    venue: "kalshi",
                    op: op.to_string(),
                });
            }
            thread::sleep(Duration::from_millis(backoff_ms(attempt)));
            continue;
        }
        return Ok((ids, status));
    }
    eprintln!("throttle limit reached");
    Err(TestErr::Throttle {
        venue: "kalshi",
        op: op.to_string(),
    })
}

fn extract_poly_order_id(resp: &str) -> Option<String> {
    let json: Value = serde_json::from_str(resp).ok()?;
    json.get("orderID")
        .or_else(|| json.get("orderId"))
        .or_else(|| json.get("id"))
        .and_then(|v| v.as_str().map(String::from))
        .or_else(|| {
            json.get("order").and_then(|o| {
                o.get("orderID")
                    .or_else(|| o.get("id"))
                    .and_then(|v| v.as_str().map(String::from))
            })
        })
}

fn poly_place_gtc(
    poly: &PolyLive,
    token_id: &str,
    price: f64,
    size: u64,
    op: &str,
) -> Result<String, TestErr> {
    let body = poly
        .build_signed_buy_limit_json(token_id, price, size, "GTC")
        .map_err(|e| TestErr::Other(format!("poly sign order: {e}")))?;
    let (status, resp) = retry_poly("polymarket", op, || poly.post_order_raw(&body))?;
    if !(200..300).contains(&status) {
        let hint = poly_order_error_hint(status, &resp);
        return Err(TestErr::Other(format!(
            "polymarket {op}: HTTP {status} {}{}",
            resp.chars().take(500).collect::<String>(),
            hint
        )));
    }
    extract_poly_order_id(&resp).ok_or_else(|| {
        TestErr::Other(format!(
            "polymarket {op}: success but no orderID in {}",
            resp.chars().take(400).collect::<String>()
        ))
    })
}

/// How to read Polymarket `/order` failures: **401/403** ≈ L2 HMAC creds; **400** ≈ signed payload.
fn poly_order_error_hint(status: u16, resp: &str) -> &'static str {
    match status {
        401 | 403 => {
            " | hint: auth/L2 — verify POLY_API_KEY, POLY_SECRET, POLY_PASSPHRASE, POLY_ADDRESS, and request path in signature"
        }
        400 if resp.contains("Invalid order payload") => {
            " | hint: not L2 HMAC per se — signature/market fields. If ETH_PRIV_KEY controls POLY_ADDRESS directly, use POLY_SIGNATURE_TYPE=0 (EOA). Polymarket.com proxy funder often needs POLY_SIGNATURE_TYPE=1. See Polymarket docs: signature types & funder."
        }
        _ => "",
    }
}

fn poly_cancel(poly: &PolyLive, order_id: &str, op: &str) -> Result<(), TestErr> {
    let body = serde_json::json!({ "orderID": order_id }).to_string();
    let (status, resp) = retry_poly("polymarket", op, || poly.delete_order_raw(&body))?;
    if !(200..300).contains(&status) {
        return Err(TestErr::Other(format!(
            "polymarket {op}: HTTP {status} {}",
            resp.chars().take(500).collect::<String>()
        )));
    }
    Ok(())
}

/// Best YES bid (USD) and best YES ask (USD) from a Kalshi WS/REST-normalized book.
fn kalshi_book_touch_usd(kb: &[(f64, f64)], ka: &[(f64, f64)]) -> (f64, Option<f64>) {
    let best_bid = kb.iter().map(|(c, _)| *c).fold(0.0_f64, f64::max) / 100.0;
    let best_ask = if ka.is_empty() {
        None
    } else {
        Some(ka.iter().map(|(c, _)| *c).fold(f64::INFINITY, f64::min) / 100.0)
    };
    (best_bid, best_ask)
}

fn log_kalshi_ws_orderbook(label: &str, kal: &KalshiLive) {
    let (bids, asks) = kal.ws_copy_orderbook();
    let (bb, ba) = kalshi_book_touch_usd(&bids, &asks);
    let ask_s = ba
        .map(|x| format!("{:.3}", x))
        .unwrap_or_else(|| "n/a".into());
    eprintln!(
        "[kalshi WS {label}] {} YES bid levels, {} YES ask levels | best bid ≈ {:.3} USD | best ask ≈ {} USD",
        bids.len(),
        asks.len(),
        bb,
        ask_s,
    );
}

fn log_poly_ws_orderbook(label: &str, poly: &PolyLive) {
    let book = poly.ws_copy_orderbook();
    let tick = poly.constraints.tick;
    let bb = book.bids.first().map(|l| l.price).unwrap_or(0.0);
    let ba = book.asks.first().map(|l| l.price).unwrap_or(1.0);
    let ba_s = if book.asks.is_empty() {
        "n/a".to_string()
    } else {
        format_poly_price_for_tick(ba, tick)
    };
    eprintln!(
        "[poly WS {label}] {} bid levels, {} ask levels | best bid {} | best ask {} USD token price",
        book.bids.len(),
        book.asks.len(),
        format_poly_price_for_tick(bb, tick),
        ba_s,
    );
}

fn run() -> Result<(), TestErr> {
    load_dotenv_override(dotenv_path().to_str().unwrap_or(".env"));
    let creds = ArbCreds::from_env();
    if creds.kalshi_api_key_id.is_empty() || creds.kalshi_private_key_pem.is_empty() {
        return Err(TestErr::Other(
            "Kalshi credentials missing (KALSHI_API_KEY_ID, KALSHI_PRIVATE_KEY_*)".into(),
        ));
    }
    if creds.poly_api_key.is_empty()
        || creds.poly_secret.is_empty()
        || creds.eth_priv_key.is_empty()
    {
        return Err(TestErr::Other(
            "Polymarket credentials missing (POLY_API_KEY, POLY_SECRET, ETH_PRIV_KEY, …)".into(),
        ));
    }

    let kal_ticker = discover_kalshi_ticker().map_err(|e| TestErr::Other(e.to_string()))?;
    let poly_pick = discover_poly_market().map_err(|e| TestErr::Other(e.to_string()))?;
    let poly_token = poly_pick.token_id.as_str();

    let mut kal =
        KalshiLive::new(&creds, &kal_ticker).map_err(|e| TestErr::Other(e.to_string()))?;
    kal.ws_connect()
        .map_err(|e| TestErr::Other(format!("kalshi WebSocket orderbook: {e}")))?;
    log_kalshi_ws_orderbook("snapshot", &kal);

    let poly_clob = PolyClobConstraints {
        neg_risk: poly_pick.neg_risk,
        order_min_size: poly_pick.base_size,
        tick: poly_pick.tick,
    };
    let mut poly = PolyLive::new(&creds, poly_token, poly_clob);
    eprintln!(
        "[poly] CLOB EIP-712 signatureType={} (env POLY_SIGNATURE_TYPE: 0=EOA, 1=POLY_PROXY, …)",
        poly.signature_type
    );
    poly.ws_connect()
        .map_err(|e| TestErr::Other(format!("polymarket WebSocket orderbook: {e}")))?;
    log_poly_ws_orderbook("snapshot", &poly);

    eprintln!(
        "[poly] test tick={} | sizes {} ⇄ {} | prices {} ⇄ {}",
        format_poly_price_for_tick(poly_pick.tick, poly_pick.tick),
        poly_pick.base_size,
        poly_pick.base_size.saturating_add(1),
        format_poly_price_for_tick(poly_pick.bid_price_low, poly_pick.tick),
        format_poly_price_for_tick(poly_pick.bid_price_high, poly_pick.tick)
    );

    // --- Kalshi: 1¢ bid, size 1 ---
    let place_batch = [KalshiBatchOrder {
        action: "buy".into(),
        count: 1,
        yes_price: 1,
        client_order_id: None,
        time_in_force: None,
    }];
    let (kal_ids, st) = retry_kalshi_batch(&kal, &place_batch, "batch_place 1¢ x1")?;
    if !(200..300).contains(&st) || kal_ids.is_empty() || kal_ids[0].is_empty() {
        return Err(TestErr::Other(format!(
            "kalshi batch_place: HTTP {st} or missing order_id"
        )));
    }
    let kal_oid = kal_ids[0].clone();
    eprintln!(
        "[kalshi] placed order {}",
        &kal_oid[..kal_oid.len().min(16)]
    );

    let poly_low = poly_pick.bid_price_low;
    let poly_high = poly_pick.bid_price_high;
    let sz_a = poly_pick.base_size;
    let sz_b = poly_pick.base_size.saturating_add(1);

    // --- Polymarket: off-market GTC bid at tick grid × min size ---
    let mut poly_oid = poly_place_gtc(
        &poly,
        poly_token,
        poly_low,
        sz_a,
        &format!("place GTC bid @{} x{}", poly_low, sz_a),
    )?;
    eprintln!(
        "[poly] placed order {}",
        &poly_oid[..poly_oid.len().min(24)]
    );

    // Size cycles: 1→2→1 (×5)
    for i in 1..=5 {
        for (label, count) in [("size→2", 2i32), ("size→1", 1i32)] {
            let (st, body) = retry_pair(
                "kalshi",
                &format!("kalshi amend size {label} cycle {i}"),
                || kal.amend_order_http(&kal_oid, "yes", "buy", 1, count),
            )?;
            if !(200..300).contains(&st) {
                return Err(TestErr::Other(format!(
                    "kalshi amend {label}: HTTP {st} {}",
                    body.chars().take(400).collect::<String>()
                )));
            }
        }
        // Poly: cancel + replace at same price 0.01
        for (label, sz) in [
            (format!("size→{}", sz_b), sz_b),
            (format!("size→{}", sz_a), sz_a),
        ] {
            poly_cancel(
                &poly,
                &poly_oid,
                &format!("poly cancel before {label} cycle {i}"),
            )?;
            poly_oid = poly_place_gtc(
                &poly,
                poly_token,
                poly_low,
                sz,
                &format!("poly replace {label} cycle {i}"),
            )?;
        }
    }

    // Price cycles on Kalshi: 1¢→2¢→1¢ (×5) (still unlikely far from market)
    for i in 1..=5 {
        for (label, price) in [("price→2¢", 2i32), ("price→1¢", 1i32)] {
            let last_count = 1i32;
            let (st, body) = retry_pair(
                "kalshi",
                &format!("kalshi amend price {label} cycle {i}"),
                || kal.amend_order_http(&kal_oid, "yes", "buy", price, last_count),
            )?;
            if !(200..300).contains(&st) {
                return Err(TestErr::Other(format!(
                    "kalshi amend {label}: HTTP {st} {}",
                    body.chars().take(400).collect::<String>()
                )));
            }
        }
    }

    // Poly price: toggle between two tick-aligned bids (×5), hold size at sz_a
    for i in 1..=5 {
        for (label, p) in [
            (format!("price→{}", poly_high), poly_high),
            (format!("price→{}", poly_low), poly_low),
        ] {
            poly_cancel(
                &poly,
                &poly_oid,
                &format!("poly cancel before {label} cycle {i}"),
            )?;
            poly_oid = poly_place_gtc(
                &poly,
                poly_token,
                p,
                sz_a,
                &format!("poly replace {label} cycle {i}"),
            )?;
        }
    }

    // Cleanup
    let _ = retry_pair("kalshi", "cancel kalshi test order", || {
        kal.cancel_order_http(&kal_oid)
    });
    let _ = poly_cancel(&poly, &poly_oid, "cancel poly test order");

    eprintln!("setup_connectivity_test: completed OK");
    Ok(())
}

/// `--swap` or `SETUP_TEST_MAKER=polymarket`: place/cancel a far Poly GTC (maker-side), then a tiny Kalshi FOK hedge.
fn run_poly_maker_smoke() -> Result<(), TestErr> {
    load_dotenv_override(dotenv_path().to_str().unwrap_or(".env"));
    let creds = ArbCreds::from_env();
    let poly_token = std::env::var("ARB_TOKEN_ID").map_err(|_| {
        TestErr::Other("ARB_TOKEN_ID required for poly maker smoke (--swap)".into())
    })?;
    let constraints = PolyClobConstraints::fetch_for_clob_token(&poly_token)
        .unwrap_or_else(|_| PolyClobConstraints::legacy_from_config(false));
    let poly = PolyLive::new(&creds, &poly_token, constraints);
    let p = floor_price_to_tick(0.01, poly.constraints.tick);
    let oid = poly_place_gtc(&poly, &poly_token, p, 5, "poly maker smoke GTC 1c x5")?;
    eprintln!(
        "[setup_test] poly maker smoke placed {}",
        &oid[..oid.len().min(16)]
    );
    poly_cancel(&poly, &oid, "poly maker smoke cancel")?;

    let ticker = if let Ok(t) = std::env::var("ARB_TICKER") {
        t
    } else {
        discover_kalshi_ticker().map_err(|e| TestErr::Other(e.to_string()))?
    };
    let kal = KalshiLive::new(&creds, &ticker).map_err(|e| TestErr::Other(e.to_string()))?;
    let hedge = [KalshiBatchOrder {
        action: "buy".into(),
        count: 1,
        yes_price: 99,
        client_order_id: None,
        time_in_force: Some("fill_or_kill".into()),
    }];
    let (_, st) = retry_kalshi_batch(&kal, &hedge, "kalshi taker smoke FOK 99c x1")?;
    if !(200..300).contains(&st) {
        return Err(TestErr::Other(format!("kalshi hedge HTTP {st}")));
    }
    eprintln!("setup_connectivity_test: poly maker smoke path OK");
    Ok(())
}

fn main() {
    let swap = std::env::args().skip(1).any(|a| a == "--swap")
        || std::env::var("SETUP_TEST_MAKER")
            .map(|v| v.eq_ignore_ascii_case("polymarket"))
            .unwrap_or(false);
    let r = if swap { run_poly_maker_smoke() } else { run() };
    if let Err(e) = r {
        eprintln!("setup_connectivity_test: FAILED: {e}");
        std::process::exit(1);
    }
}
