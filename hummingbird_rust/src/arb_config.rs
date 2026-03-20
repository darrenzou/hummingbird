use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{env, fs};

pub const MAX_CRED: usize = 512;
pub const MAX_PEM: usize = 8192;

#[derive(Debug, Clone, Default)]
pub struct ArbCreds {
    pub poly_address: String,
    pub poly_api_key: String,
    pub poly_secret: String,
    pub poly_pass: String,
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
}

pub fn load_dotenv(path: &str) {
    let _ = dotenvy::from_path(path);
}

impl ArbCreds {
    pub fn from_env() -> Self {
        let mut c = ArbCreds::default();
        c.poly_address = env::var("POLY_ADDRESS").unwrap_or_default();
        c.poly_api_key = env::var("POLY_API_KEY").unwrap_or_default();
        c.poly_secret = env::var("POLY_SECRET").unwrap_or_default();
        c.poly_pass = env::var("POLY_PASSPHRASE").unwrap_or_default();
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

        c
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

