//! Merge matched YES+NO outcome tokens into USDC.e via the Polymarket Conditional Tokens contract.
//!
//! - **When**: the poly process runs this after the same 2s post-trade debounce as presign pool refill
//!   (`RESIGN_DEBOUNCE_MS` in `arb_poly`), when there has been no new Kalshi-fill hedge since then.
//! - **Gasless path**: when `RELAY_API_KEY` and `RELAY_ADDRESS` are set (and optional host envs),
//!   submit `mergePositions` through Polymarket's relayer (`POLY_SIGNATURE_TYPE` `1` = proxy, `2` = Safe).
//! - **Direct path**: otherwise estimate gas, sign an EIP-155 legacy tx with `ETH_PRIV_KEY`, and broadcast
//!   `eth_sendRawTransaction` (signer must equal `POLY_ADDRESS` and hold positions).
//! - **Env**: `ARB_NO_TOKEN_ID` / `polymarket_no_token_id` for the NO leg; optional `POLYGON_RPC_URL`

use crate::arb_config::RelayerConfig;
use anyhow::Context;
use ethabi::{encode, Token};
use ethereum_types::H160;
use ethereum_types::U256;
use k256::ecdsa::SigningKey;
use rlp::RlpStream;
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use std::thread;
use std::time::Duration;

const CHAIN_ID: u64 = 137;
const CTF: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const USDC: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const DEFAULT_POLYGON_RPC: &str = "https://polygon-bor.publicnode.com";
const MERGE_GAS_FALLBACK: u64 = 450_000;

const SAFE_FACTORY: &str = "0xaacFeEa03eb1561C4e67d661e40682Bd20E3541b";
const PROXY_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";
const RELAY_HUB: &str = "0xD216153c06E857cD7f72665E0aF1d7D82172F494";

const SAFE_INIT_CODE_HASH: &str =
    "0x2bce2127ff07fb632d16c8347c4ebf501f4841168bed00d9e6ef715ddb6fcecf";
const PROXY_INIT_CODE_HASH: &str =
    "0xd21df8dc65880a8606f09fe0ce3df9b8869287ab0b058be05aa9e8af6330a00b";

const SAFE_TX_TYPE: &str = "SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)";
const EIP712_DOMAIN_MINIMAL: &str = "EIP712Domain(uint256 chainId,address verifyingContract)";

const PROXY_FN_SELECTOR: [u8; 4] = [0x34, 0xee, 0x97, 0x91];

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

fn zero_h160() -> H160 {
    H160::zero()
}

fn word_u256(u: U256) -> [u8; 32] {
    let mut w = [0u8; 32];
    u.to_big_endian(&mut w);
    w
}

fn word_addr(a: H160) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_bytes());
    w
}

/// Polymarket CLOB share amounts use 6 decimals (same as order `* 1_000_000` in `poly_live`).
fn float_shares_to_raw_units(size: f64) -> u128 {
    if !size.is_finite() || size <= 0.0 {
        return 0;
    }
    (size * 1_000_000.0).floor() as u128
}

/// `conditionId` for the market that contains this CLOB token (YES or NO leg).
pub fn gamma_condition_id_for_clob_token(clob_token_id: &str) -> anyhow::Result<String> {
    let url = format!("{GAMMA_BASE}/markets?clob_token_ids={clob_token_id}");
    let resp = ureq::get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| anyhow::anyhow!("gamma GET markets: {e}"))?;
    let body = resp.into_string().unwrap_or_default();
    let arr: Value = serde_json::from_str(&body).context("gamma markets: parse JSON")?;
    let first = arr
        .as_array()
        .and_then(|a| a.first())
        .context("gamma: no markets for this clob_token_id")?;
    let cid = first
        .get("conditionId")
        .and_then(|v| v.as_str())
        .context("gamma: missing conditionId")?;
    Ok(cid.to_string())
}

fn parse_hex_h160(addr: &str) -> anyhow::Result<H160> {
    let s = addr.strip_prefix("0x").unwrap_or(addr);
    let bytes = hex::decode(s).context("decode address hex")?;
    if bytes.len() != 20 {
        anyhow::bail!("address must be 20 bytes");
    }
    Ok(H160::from_slice(&bytes))
}

fn parse_hex_bytes32(s: &str) -> anyhow::Result<[u8; 32]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).context("decode condition_id hex")?;
    if bytes.len() != 32 {
        anyhow::bail!("condition_id must be 32 bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// `mergePositions(address,bytes32,bytes32,uint256[],uint256)` — binary partition `[1, 2]`.
fn encode_merge_positions_calldata(condition_id: [u8; 32], amount: U256) -> Vec<u8> {
    let mut selector = [0u8; 4];
    selector.copy_from_slice(
        &keccak256(b"mergePositions(address,bytes32,bytes32,uint256[],uint256)")[..4],
    );

    let usdc = hex::decode(USDC.strip_prefix("0x").unwrap()).expect("USDC hex");
    debug_assert_eq!(usdc.len(), 20);

    let mut head = [0u8; 160];
    head[12..32].copy_from_slice(&usdc);
    head[64..96].copy_from_slice(&condition_id);
    U256::from(160u64).to_big_endian(&mut head[96..128]);
    amount.to_big_endian(&mut head[128..160]);

    let mut tail = [0u8; 96];
    U256::from(2u64).to_big_endian(&mut tail[0..32]);
    U256::from(1u64).to_big_endian(&mut tail[32..64]);
    U256::from(2u64).to_big_endian(&mut tail[64..96]);

    let mut out = Vec::with_capacity(4 + 160 + 96);
    out.extend_from_slice(&selector);
    out.extend_from_slice(&head);
    out.extend_from_slice(&tail);
    out
}

pub fn eth_address_from_priv_key(eth_priv: &str) -> anyhow::Result<String> {
    let priv_hex = eth_priv.strip_prefix("0x").unwrap_or(eth_priv);
    let priv_bytes = hex::decode(priv_hex).context("decode ETH_PRIV_KEY")?;
    let signing_key =
        SigningKey::from_bytes(priv_bytes.as_slice().into()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let enc = signing_key.verifying_key().to_encoded_point(false);
    let pk = &enc.as_bytes()[1..];
    let h = keccak256(pk);
    Ok(format!("0x{}", hex::encode(&h[12..])))
}

fn rpc_call(rpc: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1u64,
        "method": method,
        "params": params,
    });
    let resp = ureq::post(rpc)
        .set("Content-Type", "application/json")
        .send_string(&req.to_string())
        .map_err(|e| anyhow::anyhow!("rpc {method} http: {e}"))?;
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "rpc http {status}: {}",
            &text.chars().take(240).collect::<String>()
        );
    }
    let v: Value = serde_json::from_str(&text).context("rpc json")?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            anyhow::bail!("rpc {method} error: {err}");
        }
    }
    Ok(v)
}

fn u256_from_hex_0x(s: &str) -> anyhow::Result<U256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    U256::from_str_radix(s, 16).map_err(|_| anyhow::anyhow!("invalid u256 hex"))
}

fn sign_legacy_tx_eip155(
    chain_id: u64,
    nonce: U256,
    gas_price: U256,
    gas_limit: U256,
    to: H160,
    value: U256,
    data: &[u8],
    key: &SigningKey,
) -> anyhow::Result<Vec<u8>> {
    let mut stream = RlpStream::new_list(9);
    stream.append(&nonce);
    stream.append(&gas_price);
    stream.append(&gas_limit);
    stream.append(&to);
    stream.append(&value);
    stream.append(&data);
    stream.append(&chain_id);
    stream.append(&0u8);
    stream.append(&0u8);
    let unsigned = stream.out();
    let hash = keccak256(&unsigned);

    let (sig, recid) = key
        .sign_prehash_recoverable(&hash)
        .map_err(|e| anyhow::anyhow!("tx sign: {e}"))?;
    let sig_bytes = sig.to_bytes();
    let r = U256::from_big_endian(&sig_bytes[..32]);
    let s = U256::from_big_endian(&sig_bytes[32..64]);
    let v = chain_id
        .saturating_mul(2)
        .saturating_add(35)
        .saturating_add(u64::from(recid.to_byte()));

    let mut stream = RlpStream::new_list(9);
    stream.append(&nonce);
    stream.append(&gas_price);
    stream.append(&gas_limit);
    stream.append(&to);
    stream.append(&value);
    stream.append(&data);
    stream.append(&v);
    stream.append(&r);
    stream.append(&s);
    Ok(stream.out().into())
}

fn init_code_hash32(hex_str: &str) -> anyhow::Result<[u8; 32]> {
    parse_hex_bytes32(hex_str)
}

fn create2_address(factory: H160, salt: &[u8; 32], init_code_hash: &[u8; 32]) -> H160 {
    let mut buf = vec![0xffu8];
    buf.extend_from_slice(factory.as_bytes());
    buf.extend_from_slice(salt);
    buf.extend_from_slice(init_code_hash);
    let h = keccak256(&buf);
    H160::from_slice(&h[12..])
}

fn derive_safe_address(signer_eoa: H160) -> anyhow::Result<H160> {
    let factory = parse_hex_h160(SAFE_FACTORY)?;
    let ich = init_code_hash32(SAFE_INIT_CODE_HASH)?;
    let enc = encode(&[Token::Address(signer_eoa.into())]);
    let mut salt = [0u8; 32];
    salt.copy_from_slice(&keccak256(&enc));
    Ok(create2_address(factory, &salt, &ich))
}

fn derive_proxy_wallet(signer_eoa: H160) -> anyhow::Result<H160> {
    let factory = parse_hex_h160(PROXY_FACTORY)?;
    let ich = init_code_hash32(PROXY_INIT_CODE_HASH)?;
    let mut salt = [0u8; 32];
    salt.copy_from_slice(&keccak256(signer_eoa.as_bytes()));
    Ok(create2_address(factory, &salt, &ich))
}

fn h160_token(h: H160) -> Token {
    Token::Address(h.into())
}

fn encode_proxy_wallet_calldata(ctf: H160, inner: &[u8]) -> anyhow::Result<Vec<u8>> {
    let call = Token::Tuple(vec![
        Token::Uint(1u64.into()),
        h160_token(ctf),
        Token::Uint(0u64.into()),
        Token::Bytes(inner.to_vec()),
    ]);
    let calls = Token::Array(vec![call]);
    let body = encode(&[calls]);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&PROXY_FN_SELECTOR);
    out.extend_from_slice(&body);
    Ok(out)
}

fn eip712_domain_separator(chain_id: u64, verifying_contract: H160) -> [u8; 32] {
    let type_hash = keccak256(EIP712_DOMAIN_MINIMAL.as_bytes());
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(&type_hash);
    buf.extend_from_slice(&word_u256(U256::from(chain_id)));
    buf.extend_from_slice(&word_addr(verifying_contract));
    keccak256(&buf)
}

fn hash_struct_safe_tx(
    to: H160,
    value: U256,
    data: &[u8],
    operation: u8,
    safe_tx_gas: U256,
    base_gas: U256,
    gas_price: U256,
    gas_token: H160,
    refund_receiver: H160,
    nonce: U256,
) -> [u8; 32] {
    let type_hash = keccak256(SAFE_TX_TYPE.as_bytes());
    let data_hash = keccak256(data);
    let mut buf = Vec::with_capacity(32 * 11);
    buf.extend_from_slice(&type_hash);
    buf.extend_from_slice(&word_addr(to));
    buf.extend_from_slice(&word_u256(value));
    buf.extend_from_slice(&data_hash);
    buf.extend_from_slice(&word_u256(U256::from(operation)));
    buf.extend_from_slice(&word_u256(safe_tx_gas));
    buf.extend_from_slice(&word_u256(base_gas));
    buf.extend_from_slice(&word_u256(gas_price));
    buf.extend_from_slice(&word_addr(gas_token));
    buf.extend_from_slice(&word_addr(refund_receiver));
    buf.extend_from_slice(&word_u256(nonce));
    keccak256(&buf)
}

fn eip712_typed_data_safe_digest(
    chain_id: u64,
    safe_account: H160,
    to: H160,
    value: U256,
    data: &[u8],
    operation: u8,
    nonce: U256,
) -> [u8; 32] {
    let domain_sep = eip712_domain_separator(chain_id, safe_account);
    let struct_hash = hash_struct_safe_tx(
        to,
        value,
        data,
        operation,
        U256::zero(),
        U256::zero(),
        U256::zero(),
        zero_h160(),
        zero_h160(),
        nonce,
    );
    let mut buf = [0u8; 2 + 32 + 32];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(&domain_sep);
    buf[34..66].copy_from_slice(&struct_hash);
    keccak256(&buf)
}

fn sign_gnosis_packed(key: &SigningKey, digest: &[u8; 32]) -> anyhow::Result<String> {
    let (sig, recid) = key
        .sign_prehash_recoverable(digest)
        .map_err(|e| anyhow::anyhow!("safe sig: {e}"))?;
    let b = sig.to_bytes();
    let v = u64::from(recid.to_byte()) + 31;
    Ok(format!(
        "0x{}{}{:02x}",
        hex::encode(&b[..32]),
        hex::encode(&b[32..64]),
        v
    ))
}

fn sign_eth65(key: &SigningKey, digest: &[u8; 32]) -> anyhow::Result<String> {
    let (sig, recid) = key
        .sign_prehash_recoverable(digest)
        .map_err(|e| anyhow::anyhow!("proxy sig: {e}"))?;
    let b = sig.to_bytes();
    let v = 27u64 + u64::from(recid.to_byte());
    Ok(format!(
        "0x{}{}{:02x}",
        hex::encode(&b[..32]),
        hex::encode(&b[32..64]),
        v
    ))
}

fn proxy_relay_digest(
    from_eoa: H160,
    to_factory: H160,
    data: &[u8],
    tx_fee: U256,
    gas_price: U256,
    gas_limit: U256,
    nonce: U256,
    relay_hub: H160,
    relay: H160,
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(b"rlx:");
    buf.extend_from_slice(from_eoa.as_bytes());
    buf.extend_from_slice(to_factory.as_bytes());
    buf.extend_from_slice(data);
    buf.extend_from_slice(&word_u256(tx_fee));
    buf.extend_from_slice(&word_u256(gas_price));
    buf.extend_from_slice(&word_u256(gas_limit));
    buf.extend_from_slice(&word_u256(nonce));
    buf.extend_from_slice(relay_hub.as_bytes());
    buf.extend_from_slice(relay.as_bytes());
    keccak256(&buf)
}

fn relayer_get_json(rel: &RelayerConfig, path: &str) -> anyhow::Result<Value> {
    let url = format!("{}{}", rel.base_url, path);
    let resp = ureq::get(&url)
        .set("Accept", "application/json")
        .set("RELAYER_API_KEY", &rel.api_key)
        .set("RELAYER_API_KEY_ADDRESS", &rel.api_key_address)
        .call()
        .map_err(|e| anyhow::anyhow!("relayer GET {path}: {e}"))?;
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "relayer GET {path} http {status}: {}",
            &text.chars().take(400).collect::<String>()
        );
    }
    serde_json::from_str(&text).context("relayer GET json")
}

fn relayer_post_json(rel: &RelayerConfig, path: &str, body: &Value) -> anyhow::Result<Value> {
    let url = format!("{}{}", rel.base_url, path);
    let body_str = body.to_string();
    let resp = ureq::post(&url)
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("RELAYER_API_KEY", &rel.api_key)
        .set("RELAYER_API_KEY_ADDRESS", &rel.api_key_address)
        .send_string(&body_str)
        .map_err(|e| anyhow::anyhow!("relayer POST {path}: {e}"))?;
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "relayer POST {path} http {status}: {}",
            &text.chars().take(500).collect::<String>()
        );
    }
    serde_json::from_str(&text).context("relayer POST json")
}

fn relayer_parse_nonce(v: &Value) -> anyhow::Result<U256> {
    let s = v
        .get("nonce")
        .and_then(|n| {
            n.as_str()
                .map(|s| s.to_string())
                .or_else(|| n.as_u64().map(|x| x.to_string()))
        })
        .context("relayer nonce field")?;
    if let Ok(u) = U256::from_dec_str(&s) {
        return Ok(u);
    }
    u256_from_hex_0x(&s)
}

fn relayer_transaction_array(resp: &Value) -> Vec<Value> {
    if let Some(a) = resp.as_array() {
        return a.clone();
    }
    resp.as_object()
        .and_then(|m| m.get("transactions"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

fn poll_relayer_tx_hash(rel: &RelayerConfig, transaction_id: &str) -> anyhow::Result<String> {
    let path = format!("/transaction?id={}", urlencoding::encode(transaction_id));
    for _ in 0..45usize {
        thread::sleep(Duration::from_secs(2));
        let v = match relayer_get_json(rel, &path) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[poly] relayer poll: {e:#}");
                continue;
            }
        };
        let arr = relayer_transaction_array(&v);
        let Some(first) = arr.first() else { continue };
        let state = first.get("state").and_then(|s| s.as_str()).unwrap_or("");
        if state == "STATE_FAILED" || state == "STATE_INVALID" {
            anyhow::bail!("relayer tx {transaction_id} state={state}: {first}");
        }
        if state == "STATE_MINED" || state == "STATE_CONFIRMED" {
            let h = first
                .get("transactionHash")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty() && *s != "0x");
            if let Some(txh) = h {
                return Ok(txh.to_string());
            }
        }
    }
    anyhow::bail!("relayer tx {transaction_id}: timeout waiting for STATE_MINED");
}

// --- urlencoding for query: avoid new dependency; minimal encode id ---
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}

fn merge_via_relayer(
    rel: &RelayerConfig,
    rpc_url: &str,
    eth_priv: &str,
    poly_address: &str,
    condition_id_hex: &str,
    yes_position: f64,
    no_position: f64,
    poly_sig_type: u8,
) -> anyhow::Result<Option<String>> {
    if poly_sig_type != 1 && poly_sig_type != 2 {
        anyhow::bail!(
            "relayer merge needs POLY_SIGNATURE_TYPE=1 (POLY_PROXY) or 2 (GNOSIS_SAFE); got {}",
            poly_sig_type
        );
    }

    let derived = parse_hex_h160(&eth_address_from_priv_key(eth_priv)?)?;
    let vault = if poly_sig_type == 1 {
        derive_proxy_wallet(derived)?
    } else {
        derive_safe_address(derived)?
    };
    let poly_h = parse_hex_h160(poly_address)?;
    if vault != poly_h {
        eprintln!(
            "[poly] merge skipped: POLY_ADDRESS {poly_address} != derived {:?} vault {:?} (set POLY_ADDRESS to your proxy/Safe that holds CTF positions)",
            if poly_sig_type == 1 { "PROXY" } else { "SAFE" },
            format!("0x{}", hex::encode(vault.as_bytes()))
        );
        return Ok(None);
    }

    let yes_r = float_shares_to_raw_units(yes_position);
    let no_r = float_shares_to_raw_units(no_position);
    let merge_raw = yes_r.min(no_r);
    if merge_raw == 0 {
        return Ok(None);
    }

    let amount = U256::from(merge_raw);
    let condition = parse_hex_bytes32(condition_id_hex)?;
    let merge_data = encode_merge_positions_calldata(condition, amount);
    let ctf_h = parse_hex_h160(CTF)?;

    let priv_hex = eth_priv.strip_prefix("0x").unwrap_or(eth_priv);
    let priv_bytes = hex::decode(priv_hex).context("decode priv")?;
    let signing_key =
        SigningKey::from_bytes(priv_bytes.as_slice().into()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let submit_resp = if poly_sig_type == 2 {
        let path_deployed = format!("/deployed?address=0x{}", hex::encode(vault.as_bytes()));
        let deployed = relayer_get_json(rel, &path_deployed)?;
        let is_dep = deployed
            .get("deployed")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        if !is_dep {
            eprintln!(
                "[poly] merge skipped: Safe not deployed at 0x{}",
                hex::encode(vault.as_bytes())
            );
            return Ok(None);
        }

        let nonce_path = format!(
            "/nonce?address=0x{}&type=SAFE",
            hex::encode(derived.as_bytes())
        );
        let nonce_v = relayer_get_json(rel, &nonce_path)?;
        let nonce_u = relayer_parse_nonce(&nonce_v)?;

        let digest = eip712_typed_data_safe_digest(
            CHAIN_ID,
            vault,
            ctf_h,
            U256::zero(),
            &merge_data,
            0u8,
            nonce_u,
        );
        let packed_sig = sign_gnosis_packed(&signing_key, &digest)?;

        let req = json!({
            "from": format!("0x{}", hex::encode(derived.as_bytes())),
            "to": CTF,
            "proxyWallet": format!("0x{}", hex::encode(vault.as_bytes())),
            "data": format!("0x{}", hex::encode(&merge_data)),
            "nonce": nonce_u.to_string(),
            "signature": packed_sig,
            "signatureParams": {
                "gasPrice": "0",
                "operation": "0",
                "safeTxnGas": "0",
                "baseGas": "0",
                "gasToken": "0x0000000000000000000000000000000000000000",
                "refundReceiver": "0x0000000000000000000000000000000000000000",
            },
            "type": "SAFE",
            "metadata": "",
        });
        relayer_post_json(rel, "/submit", &req)?
    } else {
        let rp_path = format!(
            "/relay-payload?address=0x{}&type=PROXY",
            hex::encode(derived.as_bytes())
        );
        let rp = relayer_get_json(rel, &rp_path)?;
        let relay_addr_str = rp
            .get("address")
            .and_then(|x| x.as_str())
            .context("relay-payload.address")?;
        let relay_h = parse_hex_h160(relay_addr_str)?;
        let nonce_u = relayer_parse_nonce(&rp)?;

        let proxy_factory_h = parse_hex_h160(PROXY_FACTORY)?;
        let relay_hub_h = parse_hex_h160(RELAY_HUB)?;
        let proxy_calldata = encode_proxy_wallet_calldata(ctf_h, &merge_data)?;

        let gas_limit = match rpc_call(
            rpc_url,
            "eth_estimateGas",
            json!([{
                "from": format!("0x{}", hex::encode(derived.as_bytes())),
                "to": PROXY_FACTORY,
                "data": format!("0x{}", hex::encode(&proxy_calldata)),
            }]),
        ) {
            Ok(v) => {
                if let Some(h) = v.get("result").and_then(|r| r.as_str()) {
                    u256_from_hex_0x(h).unwrap_or_else(|_| U256::from(10_000_000u64))
                } else {
                    U256::from(10_000_000u64)
                }
            }
            Err(_) => U256::from(10_000_000u64),
        };

        let pd = proxy_relay_digest(
            derived,
            proxy_factory_h,
            &proxy_calldata,
            U256::zero(),
            U256::zero(),
            gas_limit,
            nonce_u,
            relay_hub_h,
            relay_h,
        );
        let sig65 = sign_eth65(&signing_key, &pd)?;

        let req = json!({
            "from": format!("0x{}", hex::encode(derived.as_bytes())),
            "to": PROXY_FACTORY,
            "proxyWallet": format!("0x{}", hex::encode(vault.as_bytes())),
            "data": format!("0x{}", hex::encode(&proxy_calldata)),
            "nonce": nonce_u.to_string(),
            "signature": sig65,
            "signatureParams": {
                "gasPrice": "0",
                "gasLimit": gas_limit.to_string(),
                "relayerFee": "0",
                "relayHub": RELAY_HUB,
                "relay": relay_addr_str,
            },
            "type": "PROXY",
            "metadata": "",
        });
        relayer_post_json(rel, "/submit", &req)?
    };

    let tx_id = submit_resp
        .get("transactionID")
        .and_then(|x| x.as_str())
        .context("submit transactionID")?
        .to_string();

    eprintln!(
        "[poly] CTF merge (relayer) submitted: sets={merge_raw} id={tx_id} yes_raw={yes_r} no_raw={no_r}"
    );

    let tx_hash = poll_relayer_tx_hash(rel, &tx_id)?;
    eprintln!("[poly] CTF merge relayer mined: tx={tx_hash}");
    Ok(Some(tx_hash))
}

/// After idle debounce: merge `min(yes,no)` full sets on-chain. Returns `Ok(Some(txhash))` if a tx was sent.
pub fn try_merge_idle_yes_no(
    rpc_url: &str,
    eth_priv: &str,
    poly_address: &str,
    condition_id_hex: &str,
    yes_position: f64,
    no_position: f64,
    poly_signature_type: u8,
    relayer: Option<&RelayerConfig>,
) -> anyhow::Result<Option<String>> {
    if let Some(r) = relayer {
        return merge_via_relayer(
            r,
            rpc_url,
            eth_priv,
            poly_address,
            condition_id_hex,
            yes_position,
            no_position,
            poly_signature_type,
        );
    }

    let derived = eth_address_from_priv_key(eth_priv)?;
    if derived.to_lowercase() != poly_address.to_lowercase() {
        eprintln!(
            "[poly] merge skipped: signer {derived} != POLY_ADDRESS {poly_address} (proxy wallet?)"
        );
        return Ok(None);
    }

    let yes_r = float_shares_to_raw_units(yes_position);
    let no_r = float_shares_to_raw_units(no_position);
    let merge_raw = yes_r.min(no_r);
    if merge_raw == 0 {
        return Ok(None);
    }

    let amount = U256::from(merge_raw);
    let condition = parse_hex_bytes32(condition_id_hex)?;
    let data = encode_merge_positions_calldata(condition, amount);
    let to_ctf = parse_hex_h160(CTF)?;

    let nonce_resp = rpc_call(
        rpc_url,
        "eth_getTransactionCount",
        json!([poly_address, "pending"]),
    )?;
    let nonce_hex = nonce_resp
        .get("result")
        .and_then(|r| r.as_str())
        .context("nonce result")?;
    let nonce = u256_from_hex_0x(nonce_hex)?;

    let gas_resp = rpc_call(rpc_url, "eth_gasPrice", json!([]))?;
    let gas_price_hex = gas_resp
        .get("result")
        .and_then(|r| r.as_str())
        .context("gasPrice result")?;
    let gas_price = u256_from_hex_0x(gas_price_hex)?;

    let gas_limit = match rpc_call(
        rpc_url,
        "eth_estimateGas",
        json!([{
            "from": poly_address,
            "to": CTF,
            "data": format!("0x{}", hex::encode(&data)),
        }]),
    ) {
        Ok(v) => {
            if let Some(h) = v.get("result").and_then(|r| r.as_str()) {
                let est = u256_from_hex_0x(h).unwrap_or(U256::from(MERGE_GAS_FALLBACK));
                est.saturating_mul(U256::from(120u64)) / U256::from(100u64)
            } else {
                U256::from(MERGE_GAS_FALLBACK)
            }
        }
        Err(_) => U256::from(MERGE_GAS_FALLBACK),
    };

    let priv_hex = eth_priv.strip_prefix("0x").unwrap_or(eth_priv);
    let priv_bytes = hex::decode(priv_hex).context("decode priv")?;
    let signing_key =
        SigningKey::from_bytes(priv_bytes.as_slice().into()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let raw = sign_legacy_tx_eip155(
        CHAIN_ID,
        nonce,
        gas_price,
        gas_limit,
        to_ctf,
        U256::zero(),
        &data,
        &signing_key,
    )?;

    let raw_hex = format!("0x{}", hex::encode(&raw));
    let send = rpc_call(rpc_url, "eth_sendRawTransaction", json!([raw_hex]))?;
    let txh = send
        .get("result")
        .and_then(|r| r.as_str())
        .context("sendRawTransaction result")?
        .to_string();

    eprintln!(
        "[poly] CTF merge submitted: sets={merge_raw} units (6dp) tx={txh} yes_raw={yes_r} no_raw={no_r}"
    );
    Ok(Some(txh))
}

pub fn polygon_rpc_url() -> String {
    std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| DEFAULT_POLYGON_RPC.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eip712_safe_matches_viem_reference() {
        let safe: H160 = parse_hex_h160("0xEf86dA00A86DaFF38Af2fED48fA01E8d936d8924").unwrap();
        let to: H160 = parse_hex_h160("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045").unwrap();
        let data = hex::decode("1122").unwrap();
        let d = eip712_typed_data_safe_digest(137, safe, to, U256::zero(), &data, 0, U256::from(5));
        let want =
            parse_hex_bytes32("0x7e0796d57336f1a78638e2e0863f80f37318a27f84e51b4d0d49571260ad3414")
                .unwrap();
        assert_eq!(d, want);
    }

    #[test]
    fn proxy_calldata_matches_viem() {
        let ctf = parse_hex_h160("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045").unwrap();
        let inner = hex::decode("deadbeef").unwrap();
        let got = encode_proxy_wallet_calldata(ctf, &inner).unwrap();
        let want = hex::decode(concat!(
            "34ee9791",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000004d97dcd97ec945f40cf65f87097ace5ea0476045",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000080",
            "0000000000000000000000000000000000000000000000000000000000000004",
            "deadbeef00000000000000000000000000000000000000000000000000000000"
        ))
        .unwrap();
        assert_eq!(got, want);
    }
}
