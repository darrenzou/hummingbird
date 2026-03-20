use crate::arb_config::{now_ms, ArbCreds};
use crate::types::{PolyFullBookPayload, PriceLevel};
use base64::Engine;
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const CLOB_BASE: &str = "https://clob.polymarket.com";
const DATA_BASE: &str = "https://data-api.polymarket.com";
const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CHAIN_ID: u64 = 137;
const CTF_NEG: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
const CTF: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const FEE_BPS: &str = "0";
const SIG_TYPE: u64 = 1;

/// Parse `price` / `size` from CLOB JSON (string or number).
fn json_book_f64(obj: &serde_json::Value, key: &str) -> f64 {
    obj.get(key)
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                s.parse().ok()
            } else {
                v.as_f64()
            }
        })
        .unwrap_or(0.0)
}

/// Full orderbook via public `GET /book` (no auth, no level cap).
///
/// Sorted for `strategy` and UI: **bids** high → low (best bid first), **asks** low → high (best / lowest ask first).
pub fn fetch_clob_book_public(token_id: &str) -> anyhow::Result<PolyFullBookPayload> {
    let url = format!("{CLOB_BASE}/book");
    let resp = ureq::get(&url)
        .set("Accept", "application/json")
        .query("token_id", token_id)
        .call()
        .map_err(|e| anyhow::anyhow!("Polymarket CLOB GET /book: {e}"))?;
    let status = resp.status();
    let body = resp.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let snippet: String = body.chars().take(240).collect();
        anyhow::bail!("Polymarket CLOB /book HTTP {status}: {snippet}");
    }
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("Polymarket /book JSON: {e}"))?;
    if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("Polymarket CLOB error: {err}");
    }

    let mut bids: Vec<PriceLevel> = Vec::new();
    if let Some(arr) = json.get("bids").and_then(|a| a.as_array()) {
        for b in arr {
            let price = json_book_f64(b, "price");
            let size = json_book_f64(b, "size");
            if size > 0.0 {
                bids.push(PriceLevel { price, size });
            }
        }
    }
    bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));

    let mut asks: Vec<PriceLevel> = Vec::new();
    if let Some(arr) = json.get("asks").and_then(|a| a.as_array()) {
        for a in arr {
            let price = json_book_f64(a, "price");
            let size = json_book_f64(a, "size");
            if size > 0.0 {
                asks.push(PriceLevel { price, size });
            }
        }
    }
    asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!(
        "[poly] REST /book full snapshot: {} bids, {} asks",
        bids.len(),
        asks.len()
    );

    Ok(PolyFullBookPayload { bids, asks })
}

static SALT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_salt() -> String {
    let ts = now_ms();
    let seq = SALT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{ts}{seq:06}")
}

pub const PLACE_OK: i32 = 0;
pub const PLACE_ERR_AUTH: i32 = 1;
pub const PLACE_ERR_RATE: i32 = 2;
pub const PLACE_ERR_NETWORK: i32 = 3;
pub const PLACE_ERR_OTHER: i32 = 4;

pub struct PolyLive {
    address: String,
    api_key: String,
    secret: String,
    passphrase: String,
    eth_priv: String,
    pub token_id: String,
    neg_risk: bool,
    ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    bids_p: Vec<f64>,
    bids_s: Vec<f64>,
    asks_p: Vec<f64>,
    asks_s: Vec<f64>,
    got_snapshot: bool,
    done: bool,
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

fn abi_u64(v: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&v.to_be_bytes());
    buf
}

fn abi_dec(dec: &str) -> [u8; 32] {
    let mut buf = [0u8; 32];
    if let Ok(n) = dec.parse::<BigUint>() {
        let bytes = n.to_bytes_be();
        let len = bytes.len().min(32);
        let start = 32 - len;
        buf[start..start + len].copy_from_slice(&bytes[..len]);
    }
    buf
}

fn abi_addr(hex_addr: &str) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let hex_str = hex_addr.strip_prefix("0x").unwrap_or(hex_addr);
    if let Ok(bytes) = hex::decode(hex_str) {
        let len = bytes.len().min(32);
        let start = 32 - len;
        buf[start..start + len].copy_from_slice(&bytes[..len]);
    }
    buf
}

fn abi_str(s: &str) -> [u8; 32] {
    keccak256(s.as_bytes())
}

fn sign_order_eip712(
    token_id: &str,
    maker: &str,
    salt: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    neg_risk: bool,
    eth_priv: &str,
) -> anyhow::Result<String> {
    let domain_type = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
    let order_type = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

    let d_hash = keccak256(domain_type.as_bytes());
    let o_hash = keccak256(order_type.as_bytes());
    let exchange = if neg_risk { CTF_NEG } else { CTF };

    let mut dom_enc = Vec::with_capacity(5 * 32);
    dom_enc.extend_from_slice(&d_hash);
    dom_enc.extend_from_slice(&abi_str("CTF Exchange"));
    dom_enc.extend_from_slice(&abi_str("1"));
    dom_enc.extend_from_slice(&abi_u64(CHAIN_ID));
    dom_enc.extend_from_slice(&abi_addr(exchange));
    let dom_sep = keccak256(&dom_enc);

    let mut struct_enc = Vec::with_capacity(13 * 32);
    struct_enc.extend_from_slice(&o_hash);
    struct_enc.extend_from_slice(&abi_dec(salt));
    struct_enc.extend_from_slice(&abi_addr(maker));
    struct_enc.extend_from_slice(&abi_addr(maker));
    struct_enc.extend_from_slice(&abi_addr(
        "0x0000000000000000000000000000000000000000",
    ));
    struct_enc.extend_from_slice(&abi_dec(token_id));
    struct_enc.extend_from_slice(&abi_dec(maker_amt));
    struct_enc.extend_from_slice(&abi_dec(taker_amt));
    struct_enc.extend_from_slice(&abi_u64(0));
    struct_enc.extend_from_slice(&abi_u64(0));
    struct_enc.extend_from_slice(&abi_dec(FEE_BPS));
    struct_enc.extend_from_slice(&abi_u64(side as u64));
    struct_enc.extend_from_slice(&abi_u64(SIG_TYPE));
    let struct_hash = keccak256(&struct_enc);

    let mut pre = Vec::with_capacity(66);
    pre.push(0x19);
    pre.push(0x01);
    pre.extend_from_slice(&dom_sep);
    pre.extend_from_slice(&struct_hash);
    let digest = keccak256(&pre);

    let priv_hex = eth_priv.strip_prefix("0x").unwrap_or(eth_priv);
    let priv_bytes = hex::decode(priv_hex)?;
    let signing_key = k256::ecdsa::SigningKey::from_bytes(priv_bytes.as_slice().into())
        .map_err(|e| anyhow::anyhow!("k256 key: {e}"))?;
    let (sig, recid) = signing_key
        .sign_prehash_recoverable(&digest)
        .map_err(|e| anyhow::anyhow!("k256 sign: {e}"))?;

    let sig_bytes = sig.to_bytes();
    let r = &sig_bytes[..32];
    let s = &sig_bytes[32..64];
    let v = 27 + recid.to_byte();

    Ok(format!(
        "0x{}{}{:02x}",
        hex::encode(r),
        hex::encode(s),
        v
    ))
}

pub const POLY_BATCH_MAX: usize = 15;

fn build_order_value(
    salt: &str,
    maker: &str,
    token_id: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    sig: &str,
) -> serde_json::Value {
    serde_json::json!({
        "order": {
            "salt": salt,
            "maker": maker,
            "signer": maker,
            "taker": "0x0000000000000000000000000000000000000000",
            "tokenId": token_id,
            "makerAmount": maker_amt,
            "takerAmount": taker_amt,
            "expiration": "0",
            "nonce": "0",
            "feeRateBps": FEE_BPS,
            "side": side,
            "signatureType": SIG_TYPE as u8,
            "signature": sig,
        },
        "owner": maker,
        "orderType": "FOK",
    })
}

fn build_order_json(
    salt: &str,
    maker: &str,
    token_id: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    sig: &str,
) -> String {
    build_order_value(salt, maker, token_id, maker_amt, taker_amt, side, sig).to_string()
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

impl PolyLive {
    pub fn new(creds: &ArbCreds, token_id: &str, neg_risk: bool) -> Self {
        Self {
            address: creds.poly_address.clone(),
            api_key: creds.poly_api_key.clone(),
            secret: creds.poly_secret.clone(),
            passphrase: creds.poly_pass.clone(),
            eth_priv: creds.eth_priv_key.clone(),
            token_id: token_id.to_string(),
            neg_risk,
            ws: None,
            bids_p: Vec::new(),
            bids_s: Vec::new(),
            asks_p: Vec::new(),
            asks_s: Vec::new(),
            got_snapshot: false,
            done: false,
        }
    }

    fn hmac_sign(&self, ts: &str, method: &str, path: &str, body: &str) -> String {
        let key = base64::engine::general_purpose::STANDARD
            .decode(&self.secret)
            .unwrap_or_default();
        let msg = format!("{ts}{method}{path}{body}");
        let mut mac = <Hmac<Sha256>>::new_from_slice(&key).expect("hmac key");
        mac.update(msg.as_bytes());
        let result = mac.finalize().into_bytes();
        base64::engine::general_purpose::STANDARD.encode(&result)
    }

    fn add_auth(&self, req: ureq::Request, method: &str, path: &str, body: &str) -> ureq::Request {
        let ts = format!("{}", now_ms() / 1000);
        let sig = self.hmac_sign(&ts, method, path, body);
        req.set("POLY_ADDRESS", &self.address)
            .set("POLY_SIGNATURE", &sig)
            .set("POLY_TIMESTAMP", &ts)
            .set("POLY_API_KEY", &self.api_key)
            .set("POLY_PASSPHRASE", &self.passphrase)
            .set("Content-Type", "application/json")
    }

    fn extract_path(url: &str) -> String {
        if let Some(idx) = url.find("polymarket.com") {
            let after = &url[idx..];
            if let Some(slash) = after.find('/') {
                return after[slash..].to_string();
            }
        }
        "/".to_string()
    }

    fn http(&self, method: &str, url: &str, body: Option<&str>) -> (u16, String, bool) {
        let path = Self::extract_path(url);
        let body_str = body.unwrap_or("");
        let result = if method == "POST" {
            let req = self.add_auth(ureq::post(url), method, &path, body_str);
            req.send_string(body_str)
        } else {
            let req = self.add_auth(ureq::get(url), method, &path, body_str);
            req.call()
        };
        match result {
            Ok(resp) => {
                let s = resp.status();
                let b = resp.into_string().unwrap_or_default();
                (s, b, true)
            }
            Err(ureq::Error::Status(code, resp)) => {
                let b = resp.into_string().unwrap_or_default();
                (code, b, true)
            }
            Err(_) => (0, String::new(), false),
        }
    }

    fn http_data_get(&self, url: &str) -> String {
        match ureq::get(url).set("Accept", "application/json").call() {
            Ok(resp) => resp.into_string().unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn build_signed_buy_order(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<String> {
        let taker_amt = size * 1_000_000;
        let maker_amt = (price * size as f64 * 1e6) as u64;
        let salt = unique_salt();
        let taker_s = format!("{taker_amt}");
        let maker_s = format!("{maker_amt}");
        let sig = sign_order_eip712(
            token_id,
            &self.address,
            &salt,
            &maker_s,
            &taker_s,
            0,
            self.neg_risk,
            &self.eth_priv,
        )?;
        Ok(build_order_json(
            &salt,
            &self.address,
            token_id,
            &maker_s,
            &taker_s,
            0,
            &sig,
        ))
    }

    pub fn build_signed_sell_order(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<String> {
        let maker_amt = size * 1_000_000;
        let taker_amt = (price * size as f64 * 1e6) as u64;
        let salt = unique_salt();
        let maker_s = format!("{maker_amt}");
        let taker_s = format!("{taker_amt}");
        let sig = sign_order_eip712(
            token_id,
            &self.address,
            &salt,
            &maker_s,
            &taker_s,
            1,
            self.neg_risk,
            &self.eth_priv,
        )?;
        Ok(build_order_json(
            &salt,
            &self.address,
            token_id,
            &maker_s,
            &taker_s,
            1,
            &sig,
        ))
    }

    pub fn build_signed_buy_order_value(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let taker_amt = size * 1_000_000;
        let maker_amt = (price * size as f64 * 1e6) as u64;
        let salt = unique_salt();
        let taker_s = format!("{taker_amt}");
        let maker_s = format!("{maker_amt}");
        let sig = sign_order_eip712(
            token_id,
            &self.address,
            &salt,
            &maker_s,
            &taker_s,
            0,
            self.neg_risk,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            &self.address,
            token_id,
            &maker_s,
            &taker_s,
            0,
            &sig,
        ))
    }

    pub fn build_signed_sell_order_value(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let maker_amt = size * 1_000_000;
        let taker_amt = (price * size as f64 * 1e6) as u64;
        let salt = unique_salt();
        let maker_s = format!("{maker_amt}");
        let taker_s = format!("{taker_amt}");
        let sig = sign_order_eip712(
            token_id,
            &self.address,
            &salt,
            &maker_s,
            &taker_s,
            1,
            self.neg_risk,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            &self.address,
            token_id,
            &maker_s,
            &taker_s,
            1,
            &sig,
        ))
    }

    pub fn place_order_attempt(&self, body: &str) -> i32 {
        let url = format!("{CLOB_BASE}/order");
        let (status, resp, curl_ok) = self.http("POST", &url, Some(body));
        if !curl_ok {
            return PLACE_ERR_NETWORK;
        }
        if status == 401 || status == 403 {
            return PLACE_ERR_AUTH;
        }
        if status == 429 {
            return PLACE_ERR_RATE;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            if json.get("orderID").is_some() || json.get("id").is_some() {
                return PLACE_OK;
            }
        }
        PLACE_ERR_OTHER
    }

    pub fn place_batch_orders(&self, order_values: &[serde_json::Value]) -> i32 {
        if order_values.is_empty() {
            return PLACE_OK;
        }
        let body = serde_json::Value::Array(order_values.to_vec()).to_string();
        let url = format!("{CLOB_BASE}/orders");
        let (status, _resp, curl_ok) = self.http("POST", &url, Some(&body));
        if !curl_ok {
            return PLACE_ERR_NETWORK;
        }
        if status == 401 || status == 403 {
            return PLACE_ERR_AUTH;
        }
        if status == 429 {
            return PLACE_ERR_RATE;
        }
        if status >= 200 && status < 300 {
            return PLACE_OK;
        }
        PLACE_ERR_OTHER
    }

    pub fn get_position(&self, token_id: &str) -> f64 {
        let url = format!(
            "{DATA_BASE}/positions?user={}&sizeThreshold=0&limit=500",
            self.address
        );
        let resp = self.http_data_get(&url);
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            if let Some(arr) = json.as_array() {
                for item in arr {
                    let asset = item.get("asset").and_then(|v| v.as_str()).unwrap_or("");
                    if asset == token_id {
                        return item
                            .get("size")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                    }
                }
            }
        }
        0.0
    }

    pub fn get_balance(&self) -> f64 {
        let url = format!("{CLOB_BASE}/accounts");
        let (_, resp, _) = self.http("GET", &url, Some(""));
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp) {
            let obj = if json.is_array() {
                json.as_array().and_then(|a| a.first())
            } else {
                Some(&json)
            };
            if let Some(obj) = obj {
                if let Some(b) = obj.get("balance") {
                    return if b.is_string() {
                        b.as_str().unwrap_or("0").parse().unwrap_or(0.0)
                    } else {
                        b.as_f64().unwrap_or(0.0)
                    };
                }
            }
        }
        0.0
    }

    pub fn ws_connect(&mut self) -> anyhow::Result<()> {
        let request = WS_URL
            .into_client_request()
            .map_err(|e| anyhow::anyhow!("poly ws request: {e}"))?;
        let (mut ws, _) =
            tungstenite::connect(request).map_err(|e| anyhow::anyhow!("poly ws connect: {e}"))?;

        let sub = serde_json::json!({
            "assets_ids": [self.token_id],
            "type": "market",
            "custom_feature_enabled": true,
        });
        ws.send(Message::Text(sub.to_string()))
            .map_err(|e| anyhow::anyhow!("poly ws sub: {e}"))?;
        self.ws = Some(ws);

        let deadline = now_ms() + 15000;
        while !self.got_snapshot && !self.done && now_ms() < deadline {
            self.ws_service(50);
        }
        if !self.got_snapshot {
            anyhow::bail!("timeout waiting for Poly orderbook snapshot");
        }
        eprintln!(
            "[poly] orderbook snapshot: bids={} asks={}",
            self.bids_p.len(),
            self.asks_p.len()
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
        if text == "PONG" {
            return;
        }
        let root: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let items: Vec<&serde_json::Value> = if root.is_array() {
            root.as_array().unwrap().iter().collect()
        } else {
            vec![&root]
        };
        for m in items {
            if m.get("bids").is_some() || m.get("asks").is_some() {
                self.bids_p.clear();
                self.bids_s.clear();
                self.asks_p.clear();
                self.asks_s.clear();
                if let Some(bids) = m.get("bids").and_then(|v| v.as_array()) {
                    for b in bids {
                        let p = json_book_f64(b, "price");
                        let s = json_book_f64(b, "size");
                        if s > 0.0 {
                            self.bids_p.push(p);
                            self.bids_s.push(s);
                        }
                    }
                }
                if let Some(asks) = m.get("asks").and_then(|v| v.as_array()) {
                    for a in asks {
                        let p = json_book_f64(a, "price");
                        let s = json_book_f64(a, "size");
                        if s > 0.0 {
                            self.asks_p.push(p);
                            self.asks_s.push(s);
                        }
                    }
                }
                self.got_snapshot = true;
            } else {
                let mtype = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if mtype == "price_change" {
                    if let Some(changes) = m.get("changes").and_then(|v| v.as_array()) {
                        for c in changes {
                            let pr: f64 = c
                                .get("price")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0")
                                .parse()
                                .unwrap_or(0.0);
                            let sz: f64 = c
                                .get("size")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0")
                                .parse()
                                .unwrap_or(0.0);
                            let side = c.get("side").and_then(|v| v.as_str()).unwrap_or("");
                            if side == "BUY" {
                                self.apply_delta_bid(pr, sz);
                            } else {
                                self.apply_delta_ask(pr, sz);
                            }
                        }
                    }
                }
            }
        }
    }

    fn apply_delta_bid(&mut self, price: f64, size: f64) {
        if let Some(idx) = self.bids_p.iter().position(|&p| p == price) {
            if size <= 0.0 {
                self.bids_p.remove(idx);
                self.bids_s.remove(idx);
            } else {
                self.bids_s[idx] = size;
            }
        } else if size > 0.0 {
            self.bids_p.push(price);
            self.bids_s.push(size);
        }
    }

    fn apply_delta_ask(&mut self, price: f64, size: f64) {
        if let Some(idx) = self.asks_p.iter().position(|&p| p == price) {
            if size <= 0.0 {
                self.asks_p.remove(idx);
                self.asks_s.remove(idx);
            } else {
                self.asks_s[idx] = size;
            }
        } else if size > 0.0 {
            self.asks_p.push(price);
            self.asks_s.push(size);
        }
    }

    /// Copy the live book for strategy / IPC. Sorted: bids high→low, asks low→high (matches REST `/book`).
    pub fn ws_copy_orderbook(&self) -> PolyFullBookPayload {
        let mut bids: Vec<PriceLevel> = (0..self.bids_p.len())
            .map(|i| PriceLevel {
                price: self.bids_p[i],
                size: self.bids_s[i],
            })
            .filter(|l| l.size > 0.0)
            .collect();
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));

        let mut asks: Vec<PriceLevel> = (0..self.asks_p.len())
            .map(|i| PriceLevel {
                price: self.asks_p[i],
                size: self.asks_s[i],
            })
            .filter(|l| l.size > 0.0)
            .collect();
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        PolyFullBookPayload { bids, asks }
    }

    pub fn ws_done(&self) -> bool {
        self.done
    }
}
