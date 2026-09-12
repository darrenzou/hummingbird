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
const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CHAIN_ID: u64 = 137;
const CTF_EXCHANGE_V2: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
const CTF_NEG_EXCHANGE_V2: &str = "0xe2222d279d744050d28e00520010520000310F59";
const BYTES32_ZERO: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Round Polymarket outcome prices to **3 decimal places** (0.001) to avoid float noise
/// making best bid / best ask look identical when they differ slightly.
#[inline]
pub fn round_poly_price(p: f64) -> f64 {
    (p * 1000.0).round() / 1000.0
}

/// Fractional digits for printing / JSON-style quotes: at least **3** (e.g. `0.010` like `0.210`),
/// or more if `tick` is finer than 0.001.
#[inline]
pub fn poly_price_display_decimals(tick: f64) -> usize {
    if !(tick.is_finite() && tick > 0.0) {
        return 3;
    }
    let d_tick = (-tick.log10()).ceil().clamp(0.0, 10.0) as i32;
    (d_tick.max(3)) as usize
}

/// Format like CLOB/book prices: same width as typical quotes (`0.210`, `0.010`).
#[inline]
pub fn format_poly_price_for_tick(p: f64, tick: f64) -> String {
    let d = poly_price_display_decimals(tick);
    format!("{:.*}", d, round_poly_price(p))
}

/// After tick snapping, quantize so values use at least **3** decimal places — consistent with
/// [`round_poly_price`] / book quotes. Finer `tick` uses more precision (never fewer than tick implies).
#[inline]
pub fn normalize_poly_price(p: f64, tick: f64) -> f64 {
    if !p.is_finite() {
        return p;
    }
    if !(tick.is_finite() && tick > 0.0) {
        return round_poly_price(p);
    }
    let d_tick = (-tick.log10()).ceil().clamp(0.0, 10.0) as i32;
    let d = d_tick.max(3);
    let mul = 10f64.powi(d);
    (p * mul).round() / mul
}

/// CLOB envelope constraints for a Polymarket outcome **token** (from Gamma `markets` row).
#[derive(Debug, Clone)]
pub struct PolyClobConstraints {
    pub neg_risk: bool,
    /// Minimum contracts per order (`orderMinSize`, rounded up to integer ≥ 1).
    pub order_min_size: u64,
    /// Price grid step (`orderPriceMinTickSize`).
    pub tick: f64,
}

impl PolyClobConstraints {
    /// Fallback when Gamma is unreachable: use `config.json` / env `neg_risk` only.
    pub fn legacy_from_config(neg_risk: bool) -> Self {
        Self {
            neg_risk,
            order_min_size: 1,
            tick: 0.01,
        }
    }

    /// `GET /markets?clob_token_ids=…` — authoritative `negRisk`, `orderMinSize`, tick for signing.
    pub fn fetch_for_clob_token(clob_token_id: &str) -> anyhow::Result<Self> {
        let url = format!("{GAMMA_BASE}/markets?clob_token_ids={clob_token_id}");
        let resp = ureq::get(&url)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| anyhow::anyhow!("gamma GET markets: {e}"))?;
        let body = resp.into_string().unwrap_or_default();
        let arr: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("gamma markets JSON: {e}"))?;
        let first = arr
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow::anyhow!("gamma: no markets for this clob_token_id"))?;

        let neg_risk = first
            .get("negRisk")
            .and_then(|v| v.as_bool())
            .or_else(|| first.get("neg_risk").and_then(|v| v.as_bool()))
            .unwrap_or(false);

        let mut tick = gamma_market_f64(first, "orderPriceMinTickSize", 0.01);
        if tick <= 0.0 || !tick.is_finite() {
            tick = 0.01;
        }
        let min_raw = gamma_market_f64(first, "orderMinSize", 1.0);
        let order_min_size = min_raw.max(1.0).ceil().max(1.0) as u64;

        Ok(Self {
            neg_risk,
            order_min_size,
            tick,
        })
    }
}

fn gamma_market_f64(m: &serde_json::Value, key: &str, default: f64) -> f64 {
    m.get(key)
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str()?.parse().ok())
                .or_else(|| v.as_i64().map(|i| i as f64))
        })
        .filter(|x| x.is_finite())
        .unwrap_or(default)
}

/// Snap **buy** prices down to the CLOB tick ladder (off-grid values are rejected).
#[inline]
pub fn floor_price_to_tick(price: f64, tick: f64) -> f64 {
    if !(price.is_finite() && tick.is_finite()) || tick <= 0.0 {
        return price;
    }
    let steps = (price / tick).floor();
    let mut p = steps * tick;
    if p <= 0.0 && price > 0.0 {
        p = tick;
    }
    normalize_poly_price(p, tick)
}

/// Snap **sell** prices up to the tick ladder (capped at 0.99 on the grid).
#[inline]
pub fn ceil_price_to_tick(price: f64, tick: f64) -> f64 {
    if !(price.is_finite() && tick.is_finite()) || tick <= 0.0 {
        return price;
    }
    let steps = (price / tick).ceil();
    let mut p = steps * tick;
    let max_p = floor_price_to_tick(0.99, tick);
    if p > max_p {
        p = max_p;
    }
    normalize_poly_price(p, tick)
}

/// Greedy split of `total` contracts using allowed sizes (e.g. presign slots), descending.
/// If remainder is positive but smaller than every slot, uses the smallest slot (slight over-hedge).
pub fn decompose_order_sizes(total: u64, mut allowed: Vec<u64>) -> Vec<u64> {
    if total == 0 || allowed.is_empty() {
        return Vec::new();
    }
    allowed.sort_unstable();
    allowed.dedup();
    let mut sorted_desc: Vec<u64> = allowed.into_iter().rev().collect();
    sorted_desc.retain(|&s| s > 0);

    let mut rem = total;
    let mut out = Vec::new();
    for &s in &sorted_desc {
        while rem >= s {
            out.push(s);
            rem -= s;
        }
    }
    if rem > 0 {
        let pad = sorted_desc
            .iter()
            .rev()
            .copied()
            .find(|&s| s >= rem)
            .unwrap_or(*sorted_desc.last().unwrap());
        out.push(pad);
    }
    out
}

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
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Polymarket /book JSON: {e}"))?;
    if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("Polymarket CLOB error: {err}");
    }

    let mut bids: Vec<PriceLevel> = Vec::new();
    if let Some(arr) = json.get("bids").and_then(|a| a.as_array()) {
        for b in arr {
            let price = round_poly_price(json_book_f64(b, "price"));
            let size = json_book_f64(b, "size");
            if size > 0.0 {
                bids.push(PriceLevel { price, size });
            }
        }
    }
    bids.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut asks: Vec<PriceLevel> = Vec::new();
    if let Some(arr) = json.get("asks").and_then(|a| a.as_array()) {
        for a in arr {
            let price = round_poly_price(json_book_f64(a, "price"));
            let size = json_book_f64(a, "size");
            if size > 0.0 {
                asks.push(PriceLevel { price, size });
            }
        }
    }
    asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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
    ts.saturating_add(seq).to_string()
}

pub const PLACE_OK: i32 = 0;
pub const PLACE_ERR_AUTH: i32 = 1;
pub const PLACE_ERR_RATE: i32 = 2;
pub const PLACE_ERR_NETWORK: i32 = 3;
pub const PLACE_ERR_OTHER: i32 = 4;

pub struct PolyLive {
    address: String,
    funder_address: String,
    api_key: String,
    secret: String,
    passphrase: String,
    eth_priv: String,
    pub token_id: String,
    pub constraints: PolyClobConstraints,
    /// EIP-712 `signatureType` sent with each order (`0` = EOA … `3` = POLY_1271).
    pub signature_type: u8,
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

fn abi_bytes32(hex_value: &str) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let hex_str = hex_value.strip_prefix("0x").unwrap_or(hex_value);
    if let Ok(bytes) = hex::decode(hex_str) {
        let len = bytes.len().min(32);
        buf[..len].copy_from_slice(&bytes[..len]);
    }
    buf
}

fn sign_order_eip712(
    token_id: &str,
    maker: &str,
    signer: &str,
    salt: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    neg_risk: bool,
    signature_type: u8,
    timestamp: &str,
    metadata: &str,
    builder: &str,
    eth_priv: &str,
) -> anyhow::Result<String> {
    let domain_type =
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
    let order_type = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";

    let d_hash = keccak256(domain_type.as_bytes());
    let o_hash = keccak256(order_type.as_bytes());
    let exchange = if neg_risk {
        CTF_NEG_EXCHANGE_V2
    } else {
        CTF_EXCHANGE_V2
    };

    let mut dom_enc = Vec::with_capacity(5 * 32);
    dom_enc.extend_from_slice(&d_hash);
    dom_enc.extend_from_slice(&abi_str("Polymarket CTF Exchange"));
    dom_enc.extend_from_slice(&abi_str("2"));
    dom_enc.extend_from_slice(&abi_u64(CHAIN_ID));
    dom_enc.extend_from_slice(&abi_addr(exchange));
    let dom_sep = keccak256(&dom_enc);

    let mut struct_enc = Vec::with_capacity(12 * 32);
    struct_enc.extend_from_slice(&o_hash);
    struct_enc.extend_from_slice(&abi_dec(salt));
    struct_enc.extend_from_slice(&abi_addr(maker));
    struct_enc.extend_from_slice(&abi_addr(signer));
    struct_enc.extend_from_slice(&abi_dec(token_id));
    struct_enc.extend_from_slice(&abi_dec(maker_amt));
    struct_enc.extend_from_slice(&abi_dec(taker_amt));
    struct_enc.extend_from_slice(&abi_u64(side as u64));
    struct_enc.extend_from_slice(&abi_u64(signature_type as u64));
    struct_enc.extend_from_slice(&abi_dec(timestamp));
    struct_enc.extend_from_slice(&abi_bytes32(metadata));
    struct_enc.extend_from_slice(&abi_bytes32(builder));
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

    Ok(format!("0x{}{}{:02x}", hex::encode(r), hex::encode(s), v))
}

pub const POLY_BATCH_MAX: usize = 15;

fn build_order_value(
    salt: &str,
    maker: &str,
    signer: &str,
    owner: &str,
    token_id: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    sig: &str,
    order_type: &str,
    signature_type: u8,
    timestamp: &str,
) -> serde_json::Value {
    let salt_json = salt
        .parse::<u64>()
        .map(serde_json::Value::from)
        .unwrap_or_else(|_| serde_json::Value::String(salt.to_string()));
    let side_s = if side == 0 { "BUY" } else { "SELL" };
    serde_json::json!({
        "deferExec": false,
        "postOnly": false,
        "order": {
            "salt": salt_json,
            "maker": maker,
            "signer": signer,
            "tokenId": token_id,
            "makerAmount": maker_amt,
            "takerAmount": taker_amt,
            "side": side_s,
            "signatureType": signature_type,
            "timestamp": timestamp,
            "expiration": "0",
            "metadata": BYTES32_ZERO,
            "builder": BYTES32_ZERO,
            "signature": sig,
        },
        "owner": owner,
        "orderType": order_type,
    })
}

fn build_order_json(
    salt: &str,
    maker: &str,
    signer: &str,
    owner: &str,
    token_id: &str,
    maker_amt: &str,
    taker_amt: &str,
    side: u8,
    sig: &str,
    signature_type: u8,
    timestamp: &str,
) -> String {
    build_order_value(
        salt,
        maker,
        signer,
        owner,
        token_id,
        maker_amt,
        taker_amt,
        side,
        sig,
        "FOK",
        signature_type,
        timestamp,
    )
    .to_string()
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
    pub fn new(creds: &ArbCreds, token_id: &str, constraints: PolyClobConstraints) -> Self {
        let funder_address = if creds.poly_funder_address.is_empty() {
            creds.poly_address.clone()
        } else {
            creds.poly_funder_address.clone()
        };
        Self {
            address: creds.poly_address.clone(),
            funder_address,
            api_key: creds.poly_api_key.clone(),
            secret: creds.poly_secret.clone(),
            passphrase: creds.poly_pass.clone(),
            eth_priv: creds.eth_priv_key.clone(),
            token_id: token_id.to_string(),
            constraints,
            signature_type: creds.poly_signature_type,
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
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&self.secret))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&self.secret))
            .unwrap_or_default();
        let msg = format!("{ts}{method}{path}{body}");
        let mut mac = <Hmac<Sha256>>::new_from_slice(&key).expect("hmac key");
        mac.update(msg.as_bytes());
        let result = mac.finalize().into_bytes();
        base64::engine::general_purpose::URL_SAFE.encode(&result)
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
        let result = match method {
            "POST" => {
                let req = self.add_auth(ureq::post(url), method, &path, body_str);
                req.send_string(body_str)
            }
            "DELETE" => {
                let req = self.add_auth(ureq::delete(url), method, &path, body_str);
                if body_str.is_empty() {
                    req.call()
                } else {
                    req.send_string(body_str)
                }
            }
            _ => {
                let req = self.add_auth(ureq::get(url), method, &path, body_str);
                req.call()
            }
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

    fn clob_amounts_buy(&self, price: f64, size: u64) -> anyhow::Result<(String, String)> {
        if size < self.constraints.order_min_size {
            anyhow::bail!(
                "buy size {size} < market order_min_size {}",
                self.constraints.order_min_size
            );
        }
        let p = floor_price_to_tick(price, self.constraints.tick);
        let taker_amt = size.saturating_mul(1_000_000);
        let maker_amt = (p * size as f64 * 1e6).round() as u64;
        Ok((format!("{maker_amt}"), format!("{taker_amt}")))
    }

    fn clob_amounts_sell(&self, price: f64, size: u64) -> anyhow::Result<(String, String)> {
        if size < self.constraints.order_min_size {
            anyhow::bail!(
                "sell size {size} < market order_min_size {}",
                self.constraints.order_min_size
            );
        }
        let p = ceil_price_to_tick(price, self.constraints.tick);
        let maker_amt = size.saturating_mul(1_000_000);
        let taker_amt = (p * size as f64 * 1e6).round() as u64;
        Ok((format!("{maker_amt}"), format!("{taker_amt}")))
    }

    fn maker_address(&self) -> &str {
        &self.funder_address
    }

    fn signer_address(&self) -> &str {
        if self.signature_type == 3 {
            &self.funder_address
        } else {
            &self.address
        }
    }

    pub fn build_signed_buy_order(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<String> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_buy(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            0,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_json(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            0,
            &sig,
            self.signature_type,
            &timestamp,
        ))
    }

    pub fn build_signed_sell_order(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<String> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_sell(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            1,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_json(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            1,
            &sig,
            self.signature_type,
            &timestamp,
        ))
    }

    pub fn build_signed_buy_order_value(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_buy(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            0,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            0,
            &sig,
            "FOK",
            self.signature_type,
            &timestamp,
        ))
    }

    pub fn build_signed_sell_order_value(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_sell(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            1,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            1,
            &sig,
            "FOK",
            self.signature_type,
            &timestamp,
        ))
    }

    pub fn post_order_raw(&self, body: &str) -> (u16, String, bool) {
        let url = format!("{CLOB_BASE}/order");
        self.http("POST", &url, Some(body))
    }

    /// Cancel a resting order (`DELETE /order` with JSON `orderID`).
    pub fn delete_order_raw(&self, body: &str) -> (u16, String, bool) {
        let url = format!("{CLOB_BASE}/order");
        self.http("DELETE", &url, Some(body))
    }

    pub fn cancel_clob_order(&self, order_id: &str) -> bool {
        if order_id.is_empty() {
            return false;
        }
        let body = serde_json::json!({ "orderID": order_id }).to_string();
        let (status, _, ok) = self.delete_order_raw(&body);
        ok && status < 400
    }

    /// Shallow parse: matched / filled size for `GET /data/order/{order_id}`.
    pub fn data_order_matched_size(&self, order_id: &str) -> anyhow::Result<f64> {
        let url = format!("{CLOB_BASE}/data/order/{order_id}");
        let (status, body, ok) = self.http("GET", &url, None);
        if !ok || !(200..300).contains(&status) {
            let snippet: String = body.chars().take(160).collect();
            anyhow::bail!("data/order HTTP {status}: {snippet}");
        }
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        for key in [
            "size_matched",
            "matched",
            "filled",
            "sizeFilled",
            "filled_size",
        ] {
            if let Some(x) = v.get(key) {
                if let Some(n) = x.as_f64() {
                    return Ok(n);
                }
                if let Some(n) = x.as_u64() {
                    return Ok(n as f64);
                }
                if let Some(s) = x.as_str() {
                    if let Ok(n) = s.parse::<f64>() {
                        return Ok(n);
                    }
                }
            }
        }
        Ok(0.0)
    }

    /// Signed **buy** with explicit CLOB order type (use `GTC` for resting limits).
    pub fn build_signed_buy_limit_json(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
        order_type: &str,
    ) -> anyhow::Result<String> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_buy(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            0,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            0,
            &sig,
            order_type,
            self.signature_type,
            &timestamp,
        )
        .to_string())
    }

    /// Signed **sell** resting limit with explicit `order_type` (e.g. `GTC`).
    pub fn build_signed_sell_limit_json(
        &self,
        token_id: &str,
        price: f64,
        size: u64,
        order_type: &str,
    ) -> anyhow::Result<String> {
        let salt = unique_salt();
        let timestamp = now_ms().to_string();
        let (maker_s, taker_s) = self.clob_amounts_sell(price, size)?;
        let sig = sign_order_eip712(
            token_id,
            self.maker_address(),
            self.signer_address(),
            &salt,
            &maker_s,
            &taker_s,
            1,
            self.constraints.neg_risk,
            self.signature_type,
            &timestamp,
            BYTES32_ZERO,
            BYTES32_ZERO,
            &self.eth_priv,
        )?;
        Ok(build_order_value(
            &salt,
            self.maker_address(),
            self.signer_address(),
            &self.api_key,
            token_id,
            &maker_s,
            &taker_s,
            1,
            &sig,
            order_type,
            self.signature_type,
            &timestamp,
        )
        .to_string())
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
        match self.place_batch_orders_ids(order_values) {
            Ok(_) => PLACE_OK,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("401") || msg.contains("403") || msg.contains("auth") {
                    PLACE_ERR_AUTH
                } else if msg.contains("429") || msg.contains("rate") {
                    PLACE_ERR_RATE
                } else if msg.contains("network") {
                    PLACE_ERR_NETWORK
                } else {
                    PLACE_ERR_OTHER
                }
            }
        }
    }

    /// Batch post `/orders`; returns CLOB `orderID`s in response order.
    pub fn place_batch_orders_ids(
        &self,
        order_values: &[serde_json::Value],
    ) -> anyhow::Result<Vec<String>> {
        if order_values.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::Value::Array(order_values.to_vec()).to_string();
        let url = format!("{CLOB_BASE}/orders");
        let (status, resp, curl_ok) = self.http("POST", &url, Some(&body));
        if !curl_ok {
            anyhow::bail!("network");
        }
        if status == 401 || status == 403 {
            anyhow::bail!("auth HTTP {status}");
        }
        if status == 429 {
            anyhow::bail!("rate HTTP {status}");
        }
        if !(200..300).contains(&status) {
            let snippet: String = resp.chars().take(200).collect();
            anyhow::bail!("HTTP {status}: {snippet}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&resp).unwrap_or(serde_json::Value::Null);
        let mut ids = Vec::new();
        if let Some(arr) = json.get("orders").and_then(|v| v.as_array()) {
            for item in arr {
                let id = item
                    .get("orderID")
                    .or_else(|| item.get("order_id"))
                    .or_else(|| item.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .or_else(|| {
                        item.get("order")
                            .and_then(|o| o.get("orderID"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                ids.push(id);
            }
        }
        if ids.is_empty() && !order_values.is_empty() {
            anyhow::bail!(
                "batch response missing order ids: {}",
                resp.chars().take(120).collect::<String>()
            );
        }
        Ok(ids)
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
                        return item.get("size").and_then(|v| v.as_f64()).unwrap_or(0.0);
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
                        let p = round_poly_price(json_book_f64(b, "price"));
                        let s = json_book_f64(b, "size");
                        if s > 0.0 {
                            self.bids_p.push(p);
                            self.bids_s.push(s);
                        }
                    }
                }
                if let Some(asks) = m.get("asks").and_then(|v| v.as_array()) {
                    for a in asks {
                        let p = round_poly_price(json_book_f64(a, "price"));
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
                            let pr: f64 = round_poly_price(
                                c.get("price")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("0")
                                    .parse()
                                    .unwrap_or(0.0),
                            );
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
        let price = round_poly_price(price);
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
        let price = round_poly_price(price);
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
                price: round_poly_price(self.bids_p[i]),
                size: self.bids_s[i],
            })
            .filter(|l| l.size > 0.0)
            .collect();
        bids.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut asks: Vec<PriceLevel> = (0..self.asks_p.len())
            .map(|i| PriceLevel {
                price: round_poly_price(self.asks_p[i]),
                size: self.asks_s[i],
            })
            .filter(|l| l.size > 0.0)
            .collect();
        asks.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        PolyFullBookPayload { bids, asks }
    }

    pub fn ws_done(&self) -> bool {
        self.done
    }
}
