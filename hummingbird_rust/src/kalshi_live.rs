use crate::arb_config::{now_ms, ArbCreds};
use anyhow::Context;
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::RsaPrivateKey;
use sha2::Sha256;
use signature::{RandomizedSigner, SignatureEncoding};
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const REST_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const REST_PFX: &str = "/trade-api/v2";
const WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
const WS_PATH: &str = "/trade-api/ws/v2";
pub const BATCH_MAX: usize = 20;
const MAX_LEVELS: usize = 128;

/// `BatchCreateOrdersIndividualResponse` from Kalshi OpenAPI: `order_id` lives on nested `order`, not top-level.
fn batch_response_order_id(item: &serde_json::Value) -> String {
    item.get("order_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            item.get("order")
                .and_then(|o| o.get("order_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

#[derive(Clone)]
struct Level {
    price: i32,
    qty: i32,
}

pub struct KalshiBatchOrder {
    pub action: String,
    pub count: i32,
    pub yes_price: i32,
    pub client_order_id: Option<String>,
    /// When `Some`, sent as Kalshi `time_in_force` (e.g. `fill_or_kill`). Default: `good_till_canceled`.
    pub time_in_force: Option<String>,
}

struct PendingFill {
    count: u32,
    is_bid: bool,
    order_id: String,
}

pub struct KalshiLive {
    api_key_id: String,
    signing_key: SigningKey<Sha256>,
    ticker: String,
    ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    yes: Vec<Level>,
    no: Vec<Level>,
    got_snapshot: bool,
    done: bool,
    fills_subscribed: bool,
    pending_fills: Vec<PendingFill>,
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn set_ws_timeout(ws: &WebSocket<MaybeTlsStream<TcpStream>>, dur: Option<Duration>) {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(dur);
        }
        MaybeTlsStream::NativeTls(s) => {
            let _ = s.get_ref().set_read_timeout(dur);
        }
        _ => {}
    }
}

fn update_levels(levels: &mut Vec<Level>, price: i32, delta: i32) {
    if let Some(idx) = levels.iter().position(|l| l.price == price) {
        levels[idx].qty += delta;
        if levels[idx].qty <= 0 {
            levels.remove(idx);
        }
    } else if delta > 0 && levels.len() < MAX_LEVELS {
        levels.push(Level { price, qty: delta });
        levels.sort_by_key(|l| l.price);
    }
}

/// Legacy snapshot rows: `[[price_cents, qty], ...]` (numbers or strings).
fn levels_from_snapshot(arr: &serde_json::Value) -> Vec<Level> {
    let mut levels = Vec::new();
    if let Some(arr) = arr.as_array() {
        for item in arr {
            if let Some(pair) = item.as_array() {
                if pair.len() >= 2 {
                    let price = json_to_price_cents(&pair[0]);
                    let qty = json_to_qty_int(&pair[1]);
                    if qty > 0 {
                        if let Some(p) = price {
                            levels.push(Level { price: p, qty });
                        }
                    }
                }
            }
        }
        levels.sort_by_key(|l| l.price);
    }
    levels
}

/// `orderbook_fp` uses dollar strings (`"0.4200"` → ¢); legacy `[[42, q]]` uses integer cents.
fn json_to_price_cents(v: &serde_json::Value) -> Option<i32> {
    match v {
        serde_json::Value::String(s) => {
            let f: f64 = s.parse().ok()?;
            Some((f * 100.0).round() as i32)
        }
        serde_json::Value::Number(n) => n.as_f64().map(|f| f.round() as i32),
        _ => None,
    }
}

fn json_to_qty_int(v: &serde_json::Value) -> i32 {
    match v {
        serde_json::Value::String(s) => s.parse::<f64>().map(|f| f as i32).unwrap_or(0),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) as i32,
        _ => 0,
    }
}

/// Kalshi v2 `orderbook_fp.yes_dollars` / `no_dollars`: `[["0.4200", "100"], ...]`.
fn levels_from_fp_dollars_side(arr: &serde_json::Value) -> Vec<Level> {
    let mut levels = Vec::new();
    let Some(rows) = arr.as_array() else {
        return levels;
    };
    for row in rows {
        let Some(pair) = row.as_array() else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let Some(price) = json_to_price_cents(&pair[0]) else {
            continue;
        };
        let qty = json_to_qty_int(&pair[1]);
        if qty > 0 {
            levels.push(Level { price, qty });
        }
    }
    levels.sort_by_key(|l| l.price);
    levels
}

/// Parse orderbook into yes/no ladders. Handles yes_dollars_fp/no_dollars_fp (new API),
/// yes_dollars/no_dollars, and legacy yes/no. Missing side → empty.
fn levels_from_orderbook_fp_partial(fp: &serde_json::Value) -> (Vec<Level>, Vec<Level>) {
    let yes = fp
        .get("yes_dollars_fp")
        .or_else(|| fp.get("yes_dollars"))
        .map(levels_from_fp_dollars_side)
        .unwrap_or_default();
    let no = fp
        .get("no_dollars_fp")
        .or_else(|| fp.get("no_dollars"))
        .map(levels_from_fp_dollars_side)
        .unwrap_or_default();
    (yes, no)
}

/// Parse REST `GET /markets/{t}/orderbook` or WS snapshot `msg`.
/// WS snapshot: msg has yes_dollars_fp/no_dollars_fp at root (per Kalshi docs).
fn parse_orderbook_json_value(
    root: &serde_json::Value,
) -> anyhow::Result<(Vec<Level>, Vec<Level>)> {
    // WS snapshot: yes_dollars_fp/no_dollars_fp at root
    if root.get("yes_dollars_fp").is_some() || root.get("no_dollars_fp").is_some() {
        let (yes, no) = levels_from_orderbook_fp_partial(root);
        return Ok((yes, no));
    }
    if root.get("yes_dollars").is_some() || root.get("no_dollars").is_some() {
        let (yes, no) = levels_from_orderbook_fp_partial(root);
        return Ok((yes, no));
    }
    if let Some(fp) = root.get("orderbook_fp") {
        let (yes, no) = levels_from_orderbook_fp_partial(fp);
        if !yes.is_empty() || !no.is_empty() {
            return Ok((yes, no));
        }
    }
    if let Some(ob) = root.get("orderbook") {
        let yes = levels_from_snapshot(ob.get("yes").unwrap_or(&serde_json::Value::Null));
        let no = levels_from_snapshot(ob.get("no").unwrap_or(&serde_json::Value::Null));
        if !yes.is_empty() || !no.is_empty() {
            return Ok((yes, no));
        }
    }
    let yes = levels_from_snapshot(root.get("yes").unwrap_or(&serde_json::Value::Null));
    let no = levels_from_snapshot(root.get("no").unwrap_or(&serde_json::Value::Null));
    Ok((yes, no))
}

impl KalshiLive {
    pub fn new(creds: &ArbCreds, ticker: &str) -> anyhow::Result<Self> {
        // Accept both PKCS#8 ("BEGIN PRIVATE KEY") and PKCS#1 ("BEGIN RSA PRIVATE KEY")
        let private_key = match RsaPrivateKey::from_pkcs8_pem(&creds.kalshi_private_key_pem) {
            Ok(k) => k,
            Err(_) => RsaPrivateKey::from_pkcs1_pem(&creds.kalshi_private_key_pem)
                .map_err(|e| anyhow::anyhow!("parse Kalshi PEM (pkcs8/pkcs1): {e}"))?,
        };
        let signing_key = SigningKey::<Sha256>::new(private_key);
        Ok(Self {
            api_key_id: creds.kalshi_api_key_id.clone(),
            signing_key,
            ticker: ticker.to_string(),
            ws: None,
            yes: Vec::new(),
            no: Vec::new(),
            got_snapshot: false,
            done: false,
            fills_subscribed: false,
            pending_fills: Vec::new(),
        })
    }

    fn rsa_sign(&self, ts: &str, method: &str, path: &str) -> String {
        let clean = path.split('?').next().unwrap_or(path);
        let msg = format!("{ts}{method}{clean}");
        let sig = self
            .signing_key
            .sign_with_rng(&mut rand::rngs::OsRng, msg.as_bytes());
        b64(&sig.to_bytes())
    }

    fn add_auth(&self, req: ureq::Request, method: &str, path: &str) -> ureq::Request {
        let ts = format!("{}", now_ms());
        let sign_path = format!("{REST_PFX}{}", path.split('?').next().unwrap_or(path));
        let sig = self.rsa_sign(&ts, method, &sign_path);
        req.set("KALSHI-ACCESS-KEY", &self.api_key_id)
            .set("KALSHI-ACCESS-TIMESTAMP", &ts)
            .set("KALSHI-ACCESS-SIGNATURE", &sig)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
    }

    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let url = format!("{REST_BASE}{path}");
        let result = match method {
            "POST" => {
                let req = self.add_auth(ureq::post(&url), method, path);
                req.send_string(body.unwrap_or("{}"))
            }
            "DELETE" => {
                let req = self.add_auth(ureq::delete(&url), method, path);
                req.call()
            }
            _ => {
                let req = self.add_auth(ureq::get(&url), method, path);
                req.call()
            }
        };
        match result {
            Ok(resp) => {
                let status = resp.status();
                let b = resp.into_string().unwrap_or_default();
                (status, b)
            }
            Err(ureq::Error::Status(code, resp)) => {
                let b = resp.into_string().unwrap_or_default();
                (code, b)
            }
            Err(_) => (0, String::new()),
        }
    }

    pub fn batch_place_orders(
        &self,
        orders: &[KalshiBatchOrder],
    ) -> anyhow::Result<(Vec<String>, u16)> {
        let arr: Vec<serde_json::Value> = orders
            .iter()
            .map(|o| {
                let coid = o
                    .client_order_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let tif = o.time_in_force.as_deref().unwrap_or("good_till_canceled");
                serde_json::json!({
                    "ticker": self.ticker,
                    "side": "yes",
                    "action": o.action,
                    "count": o.count,
                    "yes_price": o.yes_price,
                    "time_in_force": tif,
                    "client_order_id": coid,
                })
            })
            .collect();
        let body = serde_json::json!({ "orders": arr }).to_string();
        let (status, resp) = self.http("POST", "/portfolio/orders/batched", Some(&body));
        let mut ids = Vec::new();
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            if let Some(arr) = json.get("orders").and_then(|o| o.as_array()) {
                for item in arr {
                    let oid = batch_response_order_id(item);
                    ids.push(oid);
                }
            }
        }
        Ok((ids, status))
    }

    pub fn cancel_order(&self, order_id: &str) -> bool {
        if order_id.is_empty() {
            return false;
        }
        let path = format!("/portfolio/orders/{order_id}");
        let (status, _) = self.http("DELETE", &path, None);
        status < 400
    }

    pub fn amend_order(
        &self,
        order_id: &str,
        side: &str,
        action: &str,
        yes_price: i32,
        count: i32,
    ) -> bool {
        if order_id.is_empty() {
            return false;
        }
        let body = serde_json::json!({
            "ticker": self.ticker,
            "side": side,
            "action": action,
            "yes_price": yes_price,
            "count": count,
        })
        .to_string();
        let path = format!("/portfolio/orders/{order_id}/amend");
        let (status, resp) = self.http("POST", &path, Some(&body));
        if status >= 400 {
            return false;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            let order = json.get("order").unwrap_or(&json);
            if let Some(s) = order.get("status").and_then(|v| v.as_str()) {
                return s != "canceled";
            }
        }
        false
    }

    /// Raw HTTP response for amend (for callers that need status codes / retry).
    pub fn amend_order_http(
        &self,
        order_id: &str,
        side: &str,
        action: &str,
        yes_price: i32,
        count: i32,
    ) -> (u16, String) {
        if order_id.is_empty() {
            return (400, "empty order_id".into());
        }
        let body = serde_json::json!({
            "ticker": self.ticker,
            "side": side,
            "action": action,
            "yes_price": yes_price,
            "count": count,
        })
        .to_string();
        let path = format!("/portfolio/orders/{order_id}/amend");
        self.http("POST", &path, Some(&body))
    }

    /// Raw HTTP for cancel (`DELETE /portfolio/orders/{id}`).
    pub fn cancel_order_http(&self, order_id: &str) -> (u16, String) {
        if order_id.is_empty() {
            return (400, "empty order_id".into());
        }
        let path = format!("/portfolio/orders/{order_id}");
        self.http("DELETE", &path, None)
    }

    pub fn get_balance(&self) -> f64 {
        let (_, resp) = self.http("GET", "/portfolio/balance", None);
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            if let Some(cents) = json
                .get("balance")
                .and_then(|b| b.get("available_balance"))
                .and_then(|v| v.as_f64())
            {
                return cents / 100.0;
            }
        }
        0.0
    }

    /// Public REST orderbook — same endpoint as `kalshi_test.py` `get_orderbook(..., auth=False)`.
    /// Supports `orderbook_fp` (`yes_dollars` / `no_dollars`) and legacy `orderbook` / flat `yes`+`no`.
    pub fn fetch_public_orderbook(
        ticker: &str,
    ) -> anyhow::Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)> {
        let path = format!("/markets/{ticker}/orderbook?depth=0");
        let url = format!("{REST_BASE}{path}");
        let result = ureq::get(&url).set("Accept", "application/json").call();
        let (status, body) = match result {
            Ok(resp) => {
                let s = resp.status();
                let b = resp.into_string().unwrap_or_default();
                (s, b)
            }
            Err(ureq::Error::Status(code, resp)) => {
                let b = resp.into_string().unwrap_or_default();
                (code, b)
            }
            Err(e) => {
                anyhow::bail!("kalshi public orderbook request failed: {e}");
            }
        };
        if status >= 400 {
            anyhow::bail!("kalshi orderbook HTTP {status}: {body}");
        }
        let root: serde_json::Value =
            serde_json::from_str(&body).context("kalshi orderbook JSON")?;
        let (yes, no) = parse_orderbook_json_value(&root)?;
        Ok(Self::levels_to_bids_asks(&yes, &no))
    }

    fn levels_to_bids_asks(yes: &[Level], no: &[Level]) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let bids: Vec<(f64, f64)> = yes.iter().map(|l| (l.price as f64, l.qty as f64)).collect();
        let asks: Vec<(f64, f64)> = no
            .iter()
            .map(|l| ((100 - l.price) as f64, l.qty as f64))
            .collect();
        (bids, asks)
    }

    pub fn ws_connect(&mut self) -> anyhow::Result<()> {
        let ts = format!("{}", now_ms());
        let sig = self.rsa_sign(&ts, "GET", WS_PATH);

        let mut request = WS_URL
            .into_client_request()
            .map_err(|e| anyhow::anyhow!("ws request: {e}"))?;
        let headers = request.headers_mut();
        headers.insert(
            "KALSHI-ACCESS-KEY",
            self.api_key_id
                .parse()
                .map_err(|_| anyhow::anyhow!("bad header"))?,
        );
        headers.insert(
            "KALSHI-ACCESS-TIMESTAMP",
            ts.parse().map_err(|_| anyhow::anyhow!("bad header"))?,
        );
        headers.insert(
            "KALSHI-ACCESS-SIGNATURE",
            sig.parse().map_err(|_| anyhow::anyhow!("bad header"))?,
        );

        let (mut ws, _) =
            tungstenite::connect(request).map_err(|e| anyhow::anyhow!("ws connect: {e}"))?;

        let sub = serde_json::json!({
            "id": 1,
            "cmd": "subscribe",
            "params": {
                "channels": ["orderbook_delta"],
                "market_ticker": self.ticker,
            }
        });
        ws.send(Message::Text(sub.to_string()))
            .map_err(|e| anyhow::anyhow!("ws send sub: {e}"))?;
        self.ws = Some(ws);

        let deadline = now_ms() + 15000;
        while !self.got_snapshot && !self.done && now_ms() < deadline {
            self.ws_service(50);
        }
        if !self.got_snapshot {
            anyhow::bail!("timeout waiting for Kalshi orderbook snapshot");
        }
        eprintln!(
            "[kalshi] orderbook snapshot: yes={} no={}",
            self.yes.len(),
            self.no.len()
        );
        Ok(())
    }

    /// If the WS snapshot left the book empty, load the same public REST book as `orderbook_web`
    /// (`GET /markets/{ticker}/orderbook?depth=0`). Kalshi WS payloads sometimes omit or use shapes
    /// that parse as empty while REST returns full `orderbook_fp`.
    pub fn seed_orderbook_from_public_rest_if_empty(&mut self) -> anyhow::Result<()> {
        if !self.yes.is_empty() || !self.no.is_empty() {
            return Ok(());
        }
        let (bids, asks) = Self::fetch_public_orderbook(&self.ticker)?;
        self.yes = bids
            .iter()
            .filter(|(_, s)| *s > 0.0)
            .map(|(p, s)| Level {
                price: *p as i32,
                qty: *s as i32,
            })
            .collect();
        self.no = asks
            .iter()
            .filter(|(_, s)| *s > 0.0)
            .map(|(p, s)| Level {
                price: (100.0 - *p) as i32,
                qty: *s as i32,
            })
            .collect();
        self.yes.sort_by_key(|l| l.price);
        self.no.sort_by_key(|l| l.price);
        eprintln!(
            "[kalshi] seeded empty WS book from public REST: yes={} no={}",
            self.yes.len(),
            self.no.len()
        );
        Ok(())
    }

    pub fn ws_service(&mut self, timeout_ms: u64) {
        let mut msgs = Vec::new();
        let mut disconnected = false;

        if let Some(ws) = &mut self.ws {
            set_ws_timeout(ws, Some(Duration::from_millis(timeout_ms.max(1))));
            match ws.read() {
                Ok(Message::Text(text)) => {
                    msgs.push(text);
                    set_ws_timeout(ws, Some(Duration::from_millis(1)));
                    loop {
                        match ws.read() {
                            Ok(Message::Text(t)) => msgs.push(t),
                            Ok(Message::Ping(data)) => {
                                let _ = ws.send(Message::Pong(data));
                            }
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(e))
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                break;
                            }
                            Err(_) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    let _ = ws.send(Message::Pong(data));
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    disconnected = true;
                }
            }
        }

        if disconnected {
            self.done = true;
        }
        for text in msgs {
            self.handle_ws_msg(&text);
        }
    }

    fn handle_ws_msg(&mut self, text: &str) {
        let root: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Kalshi sometimes batches multiple events in one JSON array; a bare array has no `type`,
        // so we used to drop the whole frame and never set `got_snapshot`.
        if let Some(arr) = root.as_array() {
            for item in arr {
                self.handle_ws_json_object(item);
            }
        } else {
            self.handle_ws_json_object(&root);
        }
    }

    fn handle_ws_json_object(&mut self, root: &serde_json::Value) {
        let msg_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "orderbook_snapshot" => {
                let msg = root.get("msg").unwrap_or(root);
                // WS snapshot: msg has yes_dollars_fp/no_dollars_fp or yes_dollars/no_dollars at root
                let (yes, no) = if msg.get("yes_dollars_fp").is_some()
                    || msg.get("no_dollars_fp").is_some()
                    || msg.get("yes_dollars").is_some()
                    || msg.get("no_dollars").is_some()
                {
                    levels_from_orderbook_fp_partial(msg)
                } else if let Some(fp) = msg.get("orderbook_fp") {
                    levels_from_orderbook_fp_partial(fp)
                } else if let Some(ob) = msg.get("orderbook") {
                    (
                        levels_from_snapshot(ob.get("yes").unwrap_or(&serde_json::Value::Null)),
                        levels_from_snapshot(ob.get("no").unwrap_or(&serde_json::Value::Null)),
                    )
                } else {
                    (
                        levels_from_snapshot(msg.get("yes").unwrap_or(&serde_json::Value::Null)),
                        levels_from_snapshot(msg.get("no").unwrap_or(&serde_json::Value::Null)),
                    )
                };
                self.yes = yes;
                self.no = no;
                self.got_snapshot = true;
            }
            "orderbook_delta" => {
                let msg = root.get("msg").unwrap_or(&root);
                // API sends price_dollars (string "0.960") or price (legacy cents)
                let price_cents: Option<i32> = msg
                    .get("price_dollars_fp")
                    .or_else(|| msg.get("price_dollars"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|f| (f * 100.0).round() as i32)
                    .or_else(|| msg.get("price").and_then(|v| v.as_f64()).map(|f| f as i32));
                // API sends delta_fp (string "-54.00") or delta (legacy)
                let delta: Option<i32> = msg
                    .get("delta_fp")
                    .or_else(|| msg.get("delta"))
                    .and_then(|v| {
                        v.as_str()
                            .and_then(|s| s.parse::<f64>().ok())
                            .or_else(|| v.as_f64())
                    })
                    .map(|f| f as i32);
                let side = msg.get("side").and_then(|v| v.as_str()).unwrap_or("");
                if let (Some(price), Some(d)) = (price_cents, delta) {
                    match side {
                        "yes" => update_levels(&mut self.yes, price, d),
                        "no" => update_levels(&mut self.no, price, d),
                        _ => {}
                    }
                }
            }
            "fill" => {
                let msg = root.get("msg").unwrap_or(&root);
                let count = msg.get("count").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
                let action = msg.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let oid = msg
                    .get("order_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.pending_fills.push(PendingFill {
                    count,
                    is_bid: action == "buy",
                    order_id: oid,
                });
            }
            "error" | "subscription_error" => {
                let detail = root
                    .get("msg")
                    .or_else(|| root.get("data"))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| root.to_string());
                eprintln!("[kalshi] WebSocket error frame: type={msg_type} {detail}");
            }
            "subscribed" | "ok" => {}
            _ => {
                // Heartbeats, command acks, future message types — ignored.
            }
        }
    }

    pub fn ws_copy_orderbook(&self) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        Self::levels_to_bids_asks(&self.yes, &self.no)
    }

    pub fn ws_subscribe_fills(&mut self) {
        if self.fills_subscribed {
            return;
        }
        if let Some(ws) = &mut self.ws {
            let sub = serde_json::json!({
                "id": 2,
                "cmd": "subscribe",
                "params": {
                    "channels": ["fill"],
                    "market_tickers": [self.ticker],
                }
            });
            if ws.send(Message::Text(sub.to_string())).is_ok() {
                self.fills_subscribed = true;
            }
        }
    }

    pub fn ws_poll_fill(&mut self) -> Option<(u32, bool, String)> {
        if self.pending_fills.is_empty() {
            return None;
        }
        let f = self.pending_fills.remove(0);
        Some((f.count, f.is_bid, f.order_id))
    }

    pub fn ws_done(&self) -> bool {
        self.done
    }
}
