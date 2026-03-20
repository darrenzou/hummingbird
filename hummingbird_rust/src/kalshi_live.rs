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

/// Parse `orderbook_fp` object into yes/no level ladders.
fn levels_from_orderbook_fp(fp: &serde_json::Value) -> Option<(Vec<Level>, Vec<Level>)> {
    let yes_d = fp.get("yes_dollars")?;
    let no_d = fp.get("no_dollars")?;
    Some((
        levels_from_fp_dollars_side(yes_d),
        levels_from_fp_dollars_side(no_d),
    ))
}

/// Parse REST `GET /markets/{t}/orderbook` or WS snapshot `msg` (same shapes as kalshi_test.py).
fn parse_orderbook_json_value(root: &serde_json::Value) -> anyhow::Result<(Vec<Level>, Vec<Level>)> {
    if let Some(fp) = root.get("orderbook_fp") {
        if let Some(pair) = levels_from_orderbook_fp(fp) {
            return Ok(pair);
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
                serde_json::json!({
                    "ticker": self.ticker,
                    "side": "yes",
                    "action": o.action,
                    "count": o.count,
                    "yes_price": o.yes_price,
                    "time_in_force": "good_till_canceled",
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
                    let oid = item
                        .get("order_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
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
    pub fn fetch_public_orderbook(ticker: &str) -> anyhow::Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)> {
        let path = format!("/markets/{ticker}/orderbook?depth=0");
        let url = format!("{REST_BASE}{path}");
        let result = ureq::get(&url)
            .set("Accept", "application/json")
            .call();
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
        let bids: Vec<(f64, f64)> = yes
            .iter()
            .map(|l| (l.price as f64, l.qty as f64))
            .collect();
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
        let msg_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "orderbook_snapshot" => {
                let msg = root.get("msg").unwrap_or(&root);
                let (yes, no) = if let Some(fp) = msg.get("orderbook_fp") {
                    if let Some(p) = levels_from_orderbook_fp(fp) {
                        p
                    } else {
                        (
                            levels_from_snapshot(
                                msg.get("yes").unwrap_or(&serde_json::Value::Null),
                            ),
                            levels_from_snapshot(
                                msg.get("no").unwrap_or(&serde_json::Value::Null),
                            ),
                        )
                    }
                } else if let Some(ob) = msg.get("orderbook") {
                    (
                        levels_from_snapshot(ob.get("yes").unwrap_or(&serde_json::Value::Null)),
                        levels_from_snapshot(ob.get("no").unwrap_or(&serde_json::Value::Null)),
                    )
                } else {
                    (
                        levels_from_snapshot(
                            msg.get("yes").unwrap_or(&serde_json::Value::Null),
                        ),
                        levels_from_snapshot(msg.get("no").unwrap_or(&serde_json::Value::Null)),
                    )
                };
                self.yes = yes;
                self.no = no;
                self.got_snapshot = true;
            }
            "orderbook_delta" => {
                let msg = root.get("msg").unwrap_or(&root);
                let price = msg.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
                let delta = msg.get("delta").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
                let side = msg.get("side").and_then(|v| v.as_str()).unwrap_or("");
                match side {
                    "yes" => update_levels(&mut self.yes, price, delta),
                    "no" => update_levels(&mut self.no, price, delta),
                    _ => {}
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
            _ => {}
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
                    "channels": ["user_fills"],
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
