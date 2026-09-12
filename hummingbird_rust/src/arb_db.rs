use anyhow::Context;
use mysql::prelude::*;
use mysql::{Opts, OptsBuilder, Pool, PooledConn, SslOpts};
use postgres::Client;
use std::collections::HashMap;
use std::path::Path;

use crate::arb_config::{now_ms, ArbCreds};
use crate::types::PriceLevel;

/// Resting **maker** venue LIMIT sizes (contracts) from the cascade, keyed by YES limit price (¢).
/// (Legacy name includes `Kalshi`; table columns unchanged.)
/// Used to annotate orderbook rows with `cascade_size` when persisting snapshots.
#[derive(Clone, Default, Debug)]
pub struct KalshiCascadeOverlay {
    bid_by_limit_cents: HashMap<i16, f64>,
    ask_by_limit_cents: HashMap<i16, f64>,
}

impl KalshiCascadeOverlay {
    pub fn from_limit_maps(
        bid_by_limit_cents: HashMap<i16, f64>,
        ask_by_limit_cents: HashMap<i16, f64>,
    ) -> Self {
        Self {
            bid_by_limit_cents,
            ask_by_limit_cents,
        }
    }

    fn qty_bid_at_book_price(&self, price_cents_f: f64) -> f64 {
        let k = (price_cents_f + 0.5) as i16;
        self.bid_by_limit_cents.get(&k).copied().unwrap_or(0.0)
    }

    fn qty_ask_at_book_price(&self, price_cents_f: f64) -> f64 {
        let k = (price_cents_f + 0.5) as i16;
        self.ask_by_limit_cents.get(&k).copied().unwrap_or(0.0)
    }
}

enum DbInner {
    Postgres(std::sync::Mutex<Client>),
    Mysql(Pool),
}

pub struct ArbDb {
    inner: DbInner,
}

impl ArbDb {
    pub fn open(creds: &ArbCreds) -> anyhow::Result<Self> {
        if !creds.database_url.is_empty() {
            return Self::open_postgres(&creds.database_url);
        }
        if creds.db_host.is_empty() {
            anyhow::bail!("set DATABASE_URL (Postgres RDS) or DB_HOST (MySQL)");
        }
        Self::open_mysql(creds)
    }

    fn open_postgres(url: &str) -> anyhow::Result<Self> {
        let needs_tls = url.contains("sslmode=require")
            || url.contains("sslmode=verify-full")
            || url.contains("sslmode=prefer");
        let client = if needs_tls {
            let tls_connector = native_tls::TlsConnector::new().context("native_tls")?;
            let connector = postgres_native_tls::MakeTlsConnector::new(tls_connector);
            Client::connect(url, connector).context("postgres connect (tls)")?
        } else {
            Client::connect(url, postgres::NoTls).context("postgres connect")?
        };
        let db = ArbDb {
            inner: DbInner::Postgres(std::sync::Mutex::new(client)),
        };
        db.run_schema_pg()?;
        Ok(db)
    }

    fn open_mysql(creds: &ArbCreds) -> anyhow::Result<Self> {
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(Some(&creds.db_host))
            .tcp_port(creds.db_port)
            .user(Some(&creds.db_user))
            .pass(Some(&creds.db_pass))
            .db_name(Some(&creds.db_name));

        if !creds.db_ssl_ca.is_empty() && Path::new(&creds.db_ssl_ca).exists() {
            let ssl = SslOpts::default()
                .with_root_cert_path(Some(std::path::PathBuf::from(&creds.db_ssl_ca)));
            builder = builder.ssl_opts(ssl);
        }

        let opts: Opts = builder.into();
        let pool = Pool::new(opts).context("mysql pool create")?;
        let db = ArbDb {
            inner: DbInner::Mysql(pool),
        };
        db.run_schema_mysql()?;
        Ok(db)
    }

    fn run_schema_pg(&self) -> anyhow::Result<()> {
        let DbInner::Postgres(m) = &self.inner else {
            return Ok(());
        };
        let mut c = m.lock().expect("pg mutex");
        c.batch_execute(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id BIGSERIAL PRIMARY KEY,
                ts_ms BIGINT NOT NULL,
                venue VARCHAR(16) NOT NULL,
                event_type VARCHAR(32) NOT NULL,
                market TEXT,
                severity VARCHAR(32),
                http_status INT,
                message TEXT,
                context_json TEXT,
                fill_price_cents INT,
                fill_amount INT,
                abort_reason TEXT
            );
            CREATE TABLE IF NOT EXISTS kalshi_book (
                id BIGSERIAL PRIMARY KEY,
                ts_ms BIGINT NOT NULL,
                market TEXT,
                book_json TEXT NOT NULL,
                trigger_event_id BIGINT REFERENCES events(id)
            );
            CREATE TABLE IF NOT EXISTS poly_book (
                id BIGSERIAL PRIMARY KEY,
                ts_ms BIGINT NOT NULL,
                market TEXT,
                book_json TEXT NOT NULL,
                trigger_event_id BIGINT REFERENCES events(id)
            );
            "#,
        )
        .context("pg schema")?;
        Ok(())
    }

    fn run_schema_mysql(&self) -> anyhow::Result<()> {
        let DbInner::Mysql(pool) = &self.inner else {
            return Ok(());
        };
        let mut conn = pool.get_conn().context("mysql get_conn")?;
        conn.query_drop(
            r#"CREATE TABLE IF NOT EXISTS events (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                ts VARCHAR(32) NOT NULL,
                action_type VARCHAR(32) NOT NULL,
                fill_price_cents INT,
                fill_amount INT,
                resize_price_cents INT,
                resize_vol_before DOUBLE,
                resize_vol_after DOUBLE,
                resize_side VARCHAR(8),
                abort_reason TEXT
            )"#,
        )
        .context("create events table")?;

        conn.query_drop(
            r#"CREATE TABLE IF NOT EXISTS kalshi_orderbook_levels (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                event_id BIGINT NOT NULL,
                side VARCHAR(8) NOT NULL,
                level_index INT NOT NULL,
                price DOUBLE NOT NULL,
                size DOUBLE NOT NULL,
                cascade_size DOUBLE NULL DEFAULT NULL,
                INDEX idx_ob_event (event_id)
            )"#,
        )
        .context("create kalshi_orderbook_levels table")?;

        conn.query_drop(
            r#"CREATE TABLE IF NOT EXISTS poly_orderbook_levels (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                event_id BIGINT NOT NULL,
                side VARCHAR(8) NOT NULL,
                level_index INT NOT NULL,
                price DOUBLE NOT NULL,
                size DOUBLE NOT NULL,
                INDEX idx_ob_event (event_id)
            )"#,
        )
        .context("create poly_orderbook_levels table")?;

        conn.query_drop(
            r#"CREATE TABLE IF NOT EXISTS kalshi_book (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                ts_ms BIGINT NOT NULL,
                market VARCHAR(256),
                book_json MEDIUMTEXT NOT NULL,
                trigger_event_id BIGINT NOT NULL,
                INDEX idx_snap_event (trigger_event_id)
            )"#,
        )
        .context("mysql kalshi_book")?;

        conn.query_drop(
            r#"CREATE TABLE IF NOT EXISTS poly_book (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                ts_ms BIGINT NOT NULL,
                market VARCHAR(256),
                book_json MEDIUMTEXT NOT NULL,
                trigger_event_id BIGINT NOT NULL,
                INDEX idx_snap_event (trigger_event_id)
            )"#,
        )
        .context("mysql poly_book")?;

        Ok(())
    }

    fn books_json(
        bids: &[PriceLevel],
        asks: &[PriceLevel],
        venue_is_kalshi: bool,
        cascade: Option<&KalshiCascadeOverlay>,
    ) -> String {
        let bid_arr: Vec<serde_json::Value> = bids
            .iter()
            .map(|l| {
                let cs = if venue_is_kalshi {
                    cascade
                        .map(|c| c.qty_bid_at_book_price(l.price))
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                serde_json::json!({
                    "price": l.price,
                    "size": l.size,
                    "cascade_size": cs,
                })
            })
            .collect();
        let ask_arr: Vec<serde_json::Value> = asks
            .iter()
            .map(|l| {
                let cs = if venue_is_kalshi {
                    cascade
                        .map(|c| c.qty_ask_at_book_price(l.price))
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                serde_json::json!({
                    "price": l.price,
                    "size": l.size,
                    "cascade_size": cs,
                })
            })
            .collect();
        serde_json::json!({ "bids": bid_arr, "asks": ask_arr }).to_string()
    }

    pub fn record_start(
        &self,
        market: &str,
        kalshi_bids: &[PriceLevel],
        kalshi_asks: &[PriceLevel],
        poly_bids: &[PriceLevel],
        poly_asks: &[PriceLevel],
        kalshi_cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        match &self.inner {
            DbInner::Postgres(m) => {
                let mut c = m.lock().expect("pg mutex");
                let ts = now_ms() as i64;
                let rows = c.query(
                    "INSERT INTO events (ts_ms, venue, event_type, market, message) VALUES ($1, 'system', 'start', $2, 'arb_start') RETURNING id",
                    &[&ts, &market],
                )?;
                let eid: i64 = rows.first().context("pg start returning id")?.get(0);
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                c.execute(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &kj, &eid],
                )?;
                c.execute(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &pj, &eid],
                )?;
                Ok(())
            }
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type) VALUES (?, 'start')",
                    (&ts,),
                )?;
                let event_id = conn.last_insert_id();
                Self::insert_kalshi_levels_mysql(
                    &mut conn,
                    event_id,
                    kalshi_bids,
                    kalshi_asks,
                    kalshi_cascade,
                )?;
                Self::insert_poly_levels_mysql(&mut conn, event_id, poly_bids, poly_asks)?;
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                conn.exec_drop(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &kj, event_id as i64),
                )?;
                conn.exec_drop(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &pj, event_id as i64),
                )?;
                Ok(())
            }
        }
    }

    fn insert_kalshi_levels_mysql(
        conn: &mut PooledConn,
        event_id: u64,
        bids: &[PriceLevel],
        asks: &[PriceLevel],
        cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        if bids.is_empty() && asks.is_empty() {
            return Ok(());
        }
        let mut params: Vec<(u64, &str, i64, f64, f64, f64)> = Vec::new();
        for (i, lvl) in bids.iter().enumerate() {
            let cs = cascade
                .map(|c| c.qty_bid_at_book_price(lvl.price))
                .unwrap_or(0.0);
            params.push((event_id, "bid", i as i64, lvl.price, lvl.size, cs));
        }
        for (i, lvl) in asks.iter().enumerate() {
            let cs = cascade
                .map(|c| c.qty_ask_at_book_price(lvl.price))
                .unwrap_or(0.0);
            params.push((event_id, "ask", i as i64, lvl.price, lvl.size, cs));
        }
        conn.exec_batch(
            "INSERT INTO kalshi_orderbook_levels (event_id, side, level_index, price, size, cascade_size) VALUES (?, ?, ?, ?, ?, ?)",
            params
                .iter()
                .map(|(eid, s, li, p, sz, cs)| (*eid, *s, *li, *p, *sz, *cs)),
        )
        .context("insert kalshi levels")?;
        Ok(())
    }

    fn insert_poly_levels_mysql(
        conn: &mut PooledConn,
        event_id: u64,
        bids: &[PriceLevel],
        asks: &[PriceLevel],
    ) -> anyhow::Result<()> {
        if bids.is_empty() && asks.is_empty() {
            return Ok(());
        }
        let mut params: Vec<(u64, &str, i64, f64, f64)> = Vec::new();
        for (i, lvl) in bids.iter().enumerate() {
            params.push((event_id, "bid", i as i64, lvl.price, lvl.size));
        }
        for (i, lvl) in asks.iter().enumerate() {
            params.push((event_id, "ask", i as i64, lvl.price, lvl.size));
        }
        conn.exec_batch(
            "INSERT INTO poly_orderbook_levels (event_id, side, level_index, price, size) VALUES (?, ?, ?, ?, ?)",
            params
                .iter()
                .map(|(eid, s, li, p, sz)| (*eid, *s, *li, *p, *sz)),
        )
        .context("insert poly levels")?;
        Ok(())
    }

    pub fn record_fill(
        &self,
        market: &str,
        kalshi_bids: &[PriceLevel],
        kalshi_asks: &[PriceLevel],
        poly_bids: &[PriceLevel],
        poly_asks: &[PriceLevel],
        fill_price_cents: i32,
        fill_amount: u32,
        kalshi_cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        match &self.inner {
            DbInner::Postgres(m) => {
                let mut c = m.lock().expect("pg mutex");
                let ts = now_ms() as i64;
                let rows = c.query(
                    "INSERT INTO events (ts_ms, venue, event_type, market, fill_price_cents, fill_amount, message) VALUES ($1, 'kalshi', 'fill', $2, $3, $4, 'kalshi_fill') RETURNING id",
                    &[&ts, &market, &fill_price_cents, &(fill_amount as i32)],
                )?;
                let eid: i64 = rows.first().context("pg fill returning id")?.get(0);
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                c.execute(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &kj, &eid],
                )?;
                c.execute(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &pj, &eid],
                )?;
                Ok(())
            }
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type, fill_price_cents, fill_amount) VALUES (?, 'fill', ?, ?)",
                    (&ts, fill_price_cents, fill_amount),
                )?;
                let event_id = conn.last_insert_id();
                Self::insert_kalshi_levels_mysql(
                    &mut conn,
                    event_id,
                    kalshi_bids,
                    kalshi_asks,
                    kalshi_cascade,
                )?;
                Self::insert_poly_levels_mysql(&mut conn, event_id, poly_bids, poly_asks)?;
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                conn.exec_drop(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &kj, event_id as i64),
                )?;
                conn.exec_drop(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &pj, event_id as i64),
                )?;
                Ok(())
            }
        }
    }

    pub fn record_error(
        &self,
        market: &str,
        venue: &str,
        kind: &str,
        message: &str,
        http_status: Option<u16>,
        retryable: bool,
        kalshi_bids: &[PriceLevel],
        kalshi_asks: &[PriceLevel],
        poly_bids: &[PriceLevel],
        poly_asks: &[PriceLevel],
        context: &serde_json::Value,
        kalshi_cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        let ctx_s = context.to_string();
        match &self.inner {
            DbInner::Postgres(m) => {
                let mut c = m.lock().expect("pg mutex");
                let ts = now_ms() as i64;
                let sev = if retryable { "retryable" } else { "fatal" };
                let http_i: Option<i32> = http_status.map(|x| x as i32);
                let rows = c.query(
                    "INSERT INTO events (ts_ms, venue, event_type, market, severity, http_status, message, context_json) VALUES ($1, $2, 'error', $3, $4, $5, $6, $7) RETURNING id",
                    &[
                        &ts,
                        &venue,
                        &market,
                        &sev,
                        &http_i,
                        &message,
                        &ctx_s,
                    ],
                )?;
                let eid: i64 = rows.first().context("pg error returning id")?.get(0);
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                c.execute(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &kj, &eid],
                )?;
                c.execute(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &pj, &eid],
                )?;
                Ok(())
            }
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                let reason = format!("error:{kind}:{message}");
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type, abort_reason) VALUES (?, 'abort', ?)",
                    (&ts, &reason),
                )?;
                let event_id = conn.last_insert_id();
                Self::insert_kalshi_levels_mysql(
                    &mut conn,
                    event_id,
                    kalshi_bids,
                    kalshi_asks,
                    kalshi_cascade,
                )?;
                Self::insert_poly_levels_mysql(&mut conn, event_id, poly_bids, poly_asks)?;
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                conn.exec_drop(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &kj, event_id as i64),
                )?;
                conn.exec_drop(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &pj, event_id as i64),
                )?;
                Ok(())
            }
        }
    }

    pub fn record_resize(
        &self,
        kalshi_bids: &[PriceLevel],
        kalshi_asks: &[PriceLevel],
        poly_bids: &[PriceLevel],
        poly_asks: &[PriceLevel],
        price_cents: i32,
        is_bid: bool,
        vol_before: f64,
        vol_after: f64,
        kalshi_cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        match &self.inner {
            DbInner::Postgres(_) => Ok(()),
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                let side_str = if is_bid { "bid" } else { "ask" };
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type, resize_price_cents, resize_vol_before, resize_vol_after, resize_side) VALUES (?, 'resize', ?, ?, ?, ?)",
                    (&ts, price_cents, vol_before, vol_after, side_str),
                )?;
                let event_id = conn.last_insert_id();
                Self::insert_kalshi_levels_mysql(
                    &mut conn,
                    event_id,
                    kalshi_bids,
                    kalshi_asks,
                    kalshi_cascade,
                )?;
                Self::insert_poly_levels_mysql(&mut conn, event_id, poly_bids, poly_asks)?;
                Ok(())
            }
        }
    }

    pub fn record_abort(
        &self,
        market: &str,
        reason: &str,
        kalshi_bids: &[PriceLevel],
        kalshi_asks: &[PriceLevel],
        poly_bids: &[PriceLevel],
        poly_asks: &[PriceLevel],
        kalshi_cascade: Option<&KalshiCascadeOverlay>,
    ) -> anyhow::Result<()> {
        match &self.inner {
            DbInner::Postgres(m) => {
                let mut c = m.lock().expect("pg mutex");
                let ts = now_ms() as i64;
                let rows = c.query(
                    "INSERT INTO events (ts_ms, venue, event_type, market, abort_reason, message) VALUES ($1, 'system', 'abort', $2, $3, $3) RETURNING id",
                    &[&ts, &market, &reason],
                )?;
                let eid: i64 = rows.first().context("pg abort returning id")?.get(0);
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                c.execute(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &kj, &eid],
                )?;
                c.execute(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &pj, &eid],
                )?;
                Ok(())
            }
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type, abort_reason) VALUES (?, 'abort', ?)",
                    (&ts, reason),
                )?;
                let event_id = conn.last_insert_id();
                Self::insert_kalshi_levels_mysql(
                    &mut conn,
                    event_id,
                    kalshi_bids,
                    kalshi_asks,
                    kalshi_cascade,
                )?;
                Self::insert_poly_levels_mysql(&mut conn, event_id, poly_bids, poly_asks)?;
                let pj = Self::books_json(poly_bids, poly_asks, false, None);
                let kj = Self::books_json(kalshi_bids, kalshi_asks, true, kalshi_cascade);
                if let Err(e) = conn.exec_drop(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &kj, event_id as i64),
                ) {
                    eprintln!("[db:record_abort.kalshi_book] {e:#}");
                }
                if let Err(e) = conn.exec_drop(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &pj, event_id as i64),
                ) {
                    eprintln!("[db:record_abort.poly_book] {e:#}");
                }
                Ok(())
            }
        }
    }

    /// Marker row for manual / CI checks (`setup_db_write` binary). Writes `events` plus empty
    /// `kalshi_book` / `poly_book` snapshots linked to the new event id.
    pub fn record_setup_test_info(
        &self,
        market: &str,
        info_message: &str,
        context: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let ctx_s = context.to_string();
        let kj = Self::books_json(&[], &[], true, None);
        let pj = Self::books_json(&[], &[], false, None);
        match &self.inner {
            DbInner::Postgres(m) => {
                let mut c = m.lock().expect("pg mutex");
                let ts = now_ms() as i64;
                let rows = c.query(
                    "INSERT INTO events (ts_ms, venue, event_type, market, message, context_json) VALUES ($1, 'system', 'setup_test', $2, $3, $4) RETURNING id",
                    &[&ts, &market, &info_message, &ctx_s],
                )?;
                let eid: i64 = rows.first().context("pg setup_test returning id")?.get(0);
                c.execute(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &kj, &eid],
                )?;
                c.execute(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES ($1, $2, $3, $4)",
                    &[&ts, &market, &pj, &eid],
                )?;
                Ok(())
            }
            DbInner::Mysql(pool) => {
                let mut conn = pool.get_conn().context("mysql get_conn")?;
                let ts = format!("{}", now_ms() / 1000);
                let abort_payload =
                    format!("setup_test|market={market}|msg={info_message}|ctx={ctx_s}");
                conn.exec_drop(
                    "INSERT INTO events (ts, action_type, abort_reason) VALUES (?, 'setup_test', ?)",
                    (&ts, &abort_payload),
                )?;
                let event_id = conn.last_insert_id();
                conn.exec_drop(
                    "INSERT INTO kalshi_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &kj, event_id as i64),
                )?;
                conn.exec_drop(
                    "INSERT INTO poly_book (ts_ms, market, book_json, trigger_event_id) VALUES (?, ?, ?, ?)",
                    (now_ms() as i64, market, &pj, event_id as i64),
                )?;
                Ok(())
            }
        }
    }
}

/// Log database write errors to stderr (`record_*` is best-effort and must not hide failures).
pub fn log_db(op: &str, result: anyhow::Result<()>) {
    if let Err(e) = result {
        eprintln!("[db:{op}] {e:#}");
    }
}
