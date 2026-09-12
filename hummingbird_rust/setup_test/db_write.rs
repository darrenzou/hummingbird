//! Historical DB smoke test. Writes a `setup_test` marker into the configured database.
//! (`DATABASE_URL` for Postgres, or `DB_HOST` / `DB_USER` / … for MySQL).
//!
//! Postgres: `events.event_type = 'setup_test'`, plus empty `kalshi_book` / `poly_book` rows.
//! MySQL: `events.action_type = 'setup_test'`, payload in `abort_reason`, plus book rows.
//!
//! ```text
//! cargo run --bin setup_db_write
//! ```
//!
//! Optional env:
//! - `SETUP_TEST_MARKET` — `events.market` (Postgres) / book row market (default: `setup_test_<ts_ms>`)
//! - `SETUP_TEST_MESSAGE` — human-readable note in `events.message` (Postgres) or inside `abort_reason` (MySQL)

use std::path::Path;

use hummingbird_rust::arb_config::{load_dotenv, now_ms, ArbCreds};
use hummingbird_rust::arb_db::ArbDb;

fn main() {
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    load_dotenv(env_path.to_str().unwrap_or(".env"));

    let creds = ArbCreds::from_env();
    if creds.database_url.is_empty() && creds.db_host.is_empty() {
        eprintln!("setup_db_write: configure DATABASE_URL (Postgres) or DB_HOST (MySQL) in .env");
        std::process::exit(1);
    }

    let db = match ArbDb::open(&creds) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("setup_db_write: ArbDb::open failed: {e:#}");
            std::process::exit(1);
        }
    };

    let ts = now_ms();
    let market = std::env::var("SETUP_TEST_MARKET").unwrap_or_else(|_| format!("setup_test_{ts}"));
    let message = std::env::var("SETUP_TEST_MESSAGE")
        .unwrap_or_else(|_| "setup_db_write connectivity ping".into());

    let ctx = serde_json::json!({
        "source": "setup_db_write",
        "ts_ms": ts,
        "note": message,
    });

    match db.record_setup_test_info(&market, &message, &ctx) {
        Ok(()) => {
            eprintln!(
                "setup_db_write: OK — wrote setup_test event + book rows for market={market:?}"
            );
        }
        Err(e) => {
            eprintln!("setup_db_write: record_setup_test_info failed: {e:#}");
            std::process::exit(1);
        }
    }
}
