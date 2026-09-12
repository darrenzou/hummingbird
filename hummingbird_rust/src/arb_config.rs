use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{env, fs};

pub const MAX_CRED: usize = 512;
pub const MAX_PEM: usize = 8192;

/// Polymarket gasless relayer (`https://relayer-v2.polymarket.com`). Headers use
/// `RELAYER_API_KEY` / `RELAYER_API_KEY_ADDRESS`; load from `RELAY_*` or `RELAYER_*` env.
#[derive(Debug, Clone)]
pub struct RelayerConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_key_address: String,
}

#[derive(Debug, Clone, Default)]
pub struct ArbCreds {
    pub poly_address: String,
    pub poly_api_key: String,
    pub poly_secret: String,
    pub poly_pass: String,
    /// Optional funder wallet for Polymarket proxy / 1271 flows. For EOA orders this is empty and
    /// `POLY_ADDRESS` is used as both signer and maker.
    pub poly_funder_address: String,
    pub eth_priv_key: String,
    pub kalshi_api_key_id: String,
    pub kalshi_private_key_path: String,
    pub kalshi_private_key_pem: String,
    pub db_host: String,
    pub db_port: u16,
    pub db_user: String,
    pub db_pass: String,
    pub db_name: String,
    pub db_ssl_ca: String,
    /// `postgresql://...` for RDS (plan). When set, Postgres is used instead of MySQL.
    pub database_url: String,
    /// Polymarket CLOB EIP-712 `signatureType`: `0` = EOA (signer is `POLY_ADDRESS`), `1` =
    /// POLY_PROXY, `2` = GNOSIS_SAFE, `3` = POLY_1271. Must match how you use `ETH_PRIV_KEY` /
    /// `POLY_ADDRESS` or orders return **400** `Invalid order payload`.
    pub poly_signature_type: u8,
    /// Gasless relayer API key (env `RELAY_API_KEY` or `RELAYER_API_KEY`).
    pub relay_api_key: String,
    /// Relayer API key owner address (env `RELAY_ADDRESS` or `RELAYER_API_KEY_ADDRESS`).
    pub relay_address: String,
    /// Relayer base URL (`POLYMARKET_RELAYER_URL`, `RELAYER_URL`, or default Polygon v2 host).
    pub polymarket_relayer_url: String,
}

pub fn load_dotenv(path: &str) {
    let _ = dotenvy::from_path(path);
}

pub fn load_dotenv_override(path: &str) {
    let _ = dotenvy::from_path_override(path);
}

impl ArbCreds {
    pub fn from_env() -> Self {
        let mut c = ArbCreds::default();
        c.poly_address = env::var("POLY_ADDRESS").unwrap_or_default();
        c.poly_api_key = env::var("POLY_API_KEY").unwrap_or_default();
        c.poly_secret = env::var("POLY_SECRET").unwrap_or_default();
        c.poly_pass = env::var("POLY_PASSPHRASE").unwrap_or_default();
        c.poly_funder_address = env::var("POLY_FUNDER_ADDRESS").unwrap_or_default();
        c.eth_priv_key = env::var("ETH_PRIV_KEY").unwrap_or_default();
        c.kalshi_api_key_id = env::var("KALSHI_API_KEY_ID").unwrap_or_default();
        c.kalshi_private_key_path = env::var("KALSHI_PRIVATE_KEY_PATH").unwrap_or_default();
        c.kalshi_private_key_pem = env::var("KALSHI_PRIVATE_KEY_PEM").unwrap_or_default();

        if c.kalshi_private_key_pem.is_empty() && !c.kalshi_private_key_path.is_empty() {
            if let Ok(bytes) = fs::read(&c.kalshi_private_key_path) {
                c.kalshi_private_key_pem = String::from_utf8_lossy(&bytes).to_string();
            }
        }

        c.db_host = env::var("DB_HOST").unwrap_or_default();
        c.db_port = env::var("DB_PORT")
            .unwrap_or_else(|_| "3306".to_string())
            .parse()
            .unwrap_or(3306);
        c.db_user = env::var("DB_USER").unwrap_or_default();
        c.db_pass = env::var("DB_PASS").unwrap_or_default();
        c.db_name = env::var("DB_NAME").unwrap_or_else(|_| "arb".to_string());
        c.db_ssl_ca = env::var("DB_SSL_CA").unwrap_or_default();
        c.database_url = env::var("DATABASE_URL").unwrap_or_default();

        c.poly_signature_type = env::var("POLY_SIGNATURE_TYPE")
            .ok()
            .and_then(|s| s.parse::<u8>().ok())
            .filter(|&x| x <= 3)
            .unwrap_or(0);

        c.relay_api_key = non_empty_env("RELAY_API_KEY")
            .or_else(|| non_empty_env("RELAYER_API_KEY"))
            .unwrap_or_default();
        c.relay_address = non_empty_env("RELAY_ADDRESS")
            .or_else(|| non_empty_env("RELAYER_API_KEY_ADDRESS"))
            .unwrap_or_default();
        c.polymarket_relayer_url = non_empty_env("POLYMARKET_RELAYER_URL")
            .or_else(|| non_empty_env("RELAYER_URL"))
            .unwrap_or_else(|| "https://relayer-v2.polymarket.com".to_string());

        c
    }

    pub fn relayer_config(&self) -> Option<RelayerConfig> {
        if self.relay_api_key.is_empty() || self.relay_address.is_empty() {
            return None;
        }
        Some(RelayerConfig {
            base_url: self
                .polymarket_relayer_url
                .trim_end_matches('/')
                .to_string(),
            api_key: self.relay_api_key.clone(),
            api_key_address: self.relay_address.clone(),
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Which venue posts resting cascade limits; the other hedges with aggressive orders.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MakerVenue {
    #[default]
    Kalshi,
    Polymarket,
}

impl MakerVenue {
    pub fn from_env_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "kalshi" => Some(Self::Kalshi),
            "polymarket" | "poly" => Some(Self::Polymarket),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbPairConfig {
    pub polymarket_token_id: String,
    #[serde(default)]
    pub polymarket_no_token_id: Option<String>,
    pub kalshi_ticker: String,
    #[serde(default)]
    pub neg_risk: bool,
    #[serde(default)]
    pub maker: MakerVenue,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArbConfig {
    #[serde(default)]
    pub db_path: Option<String>,
    #[serde(default)]
    pub side_cap: Option<f64>,
    #[serde(default)]
    pub kalshi_balance: Option<f64>,
    #[serde(default)]
    pub poly_balance: Option<f64>,
    pub pairs: Vec<ArbPairConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ConfigRoot {
    Wrapper {
        #[serde(default)]
        db_path: Option<String>,
        #[serde(default)]
        side_cap: Option<f64>,
        #[serde(default)]
        kalshi_balance: Option<f64>,
        #[serde(default)]
        poly_balance: Option<f64>,
        pairs: Vec<ArbPairConfig>,
    },
    PairsArray(Vec<ArbPairConfig>),
    SinglePair(ArbPairConfig),
}

pub fn load_config(path: &str) -> anyhow::Result<ArbConfig> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let root: serde_json::Value = serde_json::from_str(&raw).context("parse json")?;

    // Support neg_risk as boolean or number (mirrors the C fix).
    let root = normalize_neg_risk(root);

    let parsed: ConfigRoot = serde_json::from_value(root).context("deserialize config")?;
    let cfg = match parsed {
        ConfigRoot::Wrapper {
            db_path,
            side_cap,
            kalshi_balance,
            poly_balance,
            pairs,
        } => ArbConfig {
            db_path,
            side_cap,
            kalshi_balance,
            poly_balance,
            pairs,
        },
        ConfigRoot::PairsArray(pairs) => ArbConfig {
            pairs,
            ..Default::default()
        },
        ConfigRoot::SinglePair(pair) => ArbConfig {
            pairs: vec![pair],
            ..Default::default()
        },
    };

    if cfg.pairs.is_empty() {
        anyhow::bail!("no valid market pairs in config");
    }
    Ok(cfg)
}

fn normalize_neg_risk(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(mut m) => {
            if let Some(pairs) = m.get_mut("pairs") {
                *pairs = normalize_neg_risk(pairs.take());
            } else {
                // Might be single pair object.
                normalize_pair_neg_risk(&mut m);
            }
            Value::Object(m)
        }
        Value::Array(mut arr) => {
            for item in &mut arr {
                if let Value::Object(m) = item {
                    normalize_pair_neg_risk(m);
                }
            }
            Value::Array(arr)
        }
        other => other,
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `ARB_MAKER` (`kalshi` / `polymarket`) — when missing, treat Kalshi as maker (legacy default).
pub fn env_maker_is_kalshi() -> bool {
    match env::var("ARB_MAKER") {
        Ok(v) => v.trim().eq_ignore_ascii_case("kalshi"),
        Err(_) => true,
    }
}

pub fn env_maker_is_polymarket() -> bool {
    env::var("ARB_MAKER")
        .map(|v| {
            let s = v.trim().to_lowercase();
            s == "polymarket" || s == "poly"
        })
        .unwrap_or(false)
}

fn normalize_pair_neg_risk(m: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::Value;
    if let Some(nr) = m.get("neg_risk").cloned() {
        let b = match nr {
            Value::Bool(b) => b,
            Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
            _ => false,
        };
        m.insert("neg_risk".to_string(), Value::Bool(b));
    }
}
