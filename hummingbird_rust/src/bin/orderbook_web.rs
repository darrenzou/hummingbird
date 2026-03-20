use anyhow::Context;
use hummingbird_rust::arb_config::{self, ArbConfig};
use hummingbird_rust::strategy::{build_cascade, cascade_empty_explanation, KalshiState};
use hummingbird_rust::kalshi_live::KalshiLive;
use hummingbird_rust::poly_live::PolyLive;
use hummingbird_rust::types::{PolyFullBookPayload, PriceLevel};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

fn render_html(
    cfg: &ArbConfig,
    kalshi: &KalshiState,
    poly: &PolyFullBookPayload,
    cascade_note: &str,
) -> String {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Orderbooks + Cascade</title>");
    html.push_str("<style>\
body{font-family:Arial,Helvetica,sans-serif;margin:20px;}\
.tables-row{display:flex;flex-wrap:wrap;gap:20px;align-items:flex-start;margin:16px 0;}\
.table-panel{flex:1 1 280px;min-width:240px;max-width:100%;}\
.table-panel h2{margin:0 0 10px 0;font-size:1.1em;}\
table{border-collapse:collapse;width:100%;margin:0;}\
th,td{border:1px solid #ccc;padding:4px 8px;text-align:right;font-size:13px;}\
th{text-align:center;background:#f5f5f5;}\
h2{margin-top:24px;}\
</style>");
    html.push_str("</head><body>");

    html.push_str("<h1>Hummingbird Rust – Orderbooks & Cascade</h1>");

    // Config summary.
    html.push_str("<h2>Config</h2><ul>");
    html.push_str(&format!(
        "<li>db_path: {:?}</li>",
        cfg.db_path.as_deref().unwrap_or("arb_events.db")
    ));
    html.push_str(&format!(
        "<li>side_cap: {:.2}</li>",
        cfg.side_cap.unwrap_or(1000.0)
    ));
    html.push_str(&format!(
        "<li>kalshi_balance: {:.2}</li>",
        cfg.kalshi_balance.unwrap_or(2000.0)
    ));
    html.push_str(&format!(
        "<li>poly_balance: {:.2}</li>",
        cfg.poly_balance.unwrap_or(2000.0)
    ));
    if let Some(pair0) = cfg.pairs.get(0) {
        html.push_str(&format!(
            "<li>pair: kalshi={} polymarket_token_id={} neg_risk={}</li>",
            pair0.kalshi_ticker, pair0.polymarket_token_id, pair0.neg_risk
        ));
    }
    html.push_str("</ul>");

    // Kalshi / Poly / Cascade side by side.
    html.push_str("<div class=\"tables-row\">");

    html.push_str("<div class=\"table-panel\"><h2>Kalshi (snapshot)</h2>");
    html.push_str("<table><tr><th colspan=\"2\">Yes Bids</th><th></th><th colspan=\"2\">Yes Asks</th></tr>");
    html.push_str("<tr><th>Price (¢)</th><th>Size</th><th></th><th>Price (¢)</th><th>Size</th></tr>");
    let rows = kalshi.yes_bids.len().max(kalshi.yes_asks.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some(b) = kalshi.yes_bids.get(i) {
            html.push_str(&format!("<td>{:.0}</td><td>{:.2}</td>", b.price, b.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some(a) = kalshi.yes_asks.get(i) {
            html.push_str(&format!("<td>{:.0}</td><td>{:.2}</td>", a.price, a.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div>");

    html.push_str("<div class=\"table-panel\"><h2>Polymarket (WebSocket, uncapped + deltas)</h2>");
    html.push_str("<p style=\"margin:0 0 8px 0;font-size:12px;color:#444;\">Refresh the page for updates. Bids: highest at top. Asks: lowest (best ask) at top.</p>");
    html.push_str("<table><tr><th colspan=\"2\">Bids</th><th></th><th colspan=\"2\">Asks</th></tr>");
    html.push_str("<tr><th>Price ($)</th><th>Size</th><th></th><th>Price ($)</th><th>Size</th></tr>");
    let rows = poly.bids.len().max(poly.asks.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some(b) = poly.bids.get(i) {
            html.push_str(&format!("<td>{:.3}</td><td>{:.2}</td>", b.price, b.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some(a) = poly.asks.get(i) {
            html.push_str(&format!("<td>{:.3}</td><td>{:.2}</td>", a.price, a.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div>");

    // Cascade levels (same rules as arb: Poly book + Kalshi ladder + portfolio cap).
    let cascade = build_cascade(kalshi, poly);
    let levels = &cascade.levels;
    html.push_str("<div class=\"table-panel\"><h2>Cascade (build_cascade)</h2>");
    html.push_str("<table><tr><th colspan=\"2\">Bids</th><th></th><th colspan=\"2\">Asks</th></tr>");
    html.push_str("<tr><th>Price (¢)</th><th>Volume</th><th></th><th>Price (¢)</th><th>Volume</th></tr>");
    let rows = levels
        .bid_levels
        .len()
        .max(levels.ask_levels.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some((pc, v)) = levels.bid_levels.get(i) {
            html.push_str(&format!("<td>{}</td><td>{:.2}</td>", pc, v));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some((pc, v)) = levels.ask_levels.get(i) {
            html.push_str(&format!("<td>{}</td><td>{:.2}</td>", pc, v));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div></div>");

    if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        let esc = cascade_note
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        html.push_str("<h3>Why the cascade table is empty</h3>");
        html.push_str("<pre style=\"white-space:pre-wrap;font-size:13px;background:#f5f5f5;padding:12px;border:1px solid #ddd;\">");
        html.push_str(&esc);
        html.push_str("</pre>");
    }

    html.push_str("</body></html>");
    html
}

fn main() -> anyhow::Result<()> {
    // Match main.rs: load .env, then config.
    arb_config::load_dotenv(".env");
    let creds = arb_config::ArbCreds::from_env();
    let cfg = arb_config::load_config("config.json").context("load config.json")?;

    let pair0 = cfg
        .pairs
        .get(0)
        .context("config must include at least one pair")?;

    // Kalshi orderbook: use public REST (same as kalshi_test.py get_orderbook auth=False).
    // WS snapshots often use `orderbook_fp`; we also parse that in kalshi_live for the arb process.
    let (bids_raw, asks_raw) = KalshiLive::fetch_public_orderbook(&pair0.kalshi_ticker).context(
        "Kalshi REST orderbook GET /markets/{ticker}/orderbook — check ticker in config.json",
    )?;

    let mut kalshi_state = KalshiState::init();
    // Live balance often returns 0 if auth/parse fails; cascade uses portfolio = min(kalshi, poly)
    // so 0 wipes all sizes. For this preview page, fall back like poly_balance.
    kalshi_state.kalshi_balance = match cfg.kalshi_balance {
        Some(kb) => kb,
        None => {
            let kl = KalshiLive::new(&creds, &pair0.kalshi_ticker)?;
            let live = kl.get_balance();
            if live > 0.0 {
                live
            } else {
                eprintln!(
                    "[orderbook_web] Kalshi get_balance() was 0 — using $2000 for cascade preview (set kalshi_balance in config.json to override)"
                );
                2000.0
            }
        }
    };
    kalshi_state.poly_balance = cfg.poly_balance.unwrap_or(2000.0);
    kalshi_state.side_cap = cfg.side_cap.unwrap_or(1000.0);

    kalshi_state.yes_bids = bids_raw
        .iter()
        .map(|&(p, s)| PriceLevel {
            price: p,
            size: s,
        })
        .collect();
    kalshi_state.yes_asks = asks_raw
        .iter()
        .map(|&(p, s)| PriceLevel {
            price: p,
            size: s,
        })
        .collect();
    kalshi_state
        .yes_bids
        .sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    kalshi_state
        .yes_asks
        .sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

    // Polymarket: WS snapshot + deltas on a dedicated thread. **Do not** wrap `PolyLive` in the same
    // mutex HTTP uses: `ws_service` can run a long time while draining many messages, which would
    // block the browser on `lock()` forever. Publish only `PolyFullBookPayload` with a short lock.
    let mut poly_live = PolyLive::new(&creds, &pair0.polymarket_token_id, pair0.neg_risk);
    poly_live
        .ws_connect()
        .context("Polymarket WebSocket failed (check token id / .env CLOB creds / network)")?;
    let poly_book_shared = Arc::new(Mutex::new(poly_live.ws_copy_orderbook()));
    let poly_book_ws = Arc::clone(&poly_book_shared);
    thread::spawn(move || {
        let mut live = poly_live;
        loop {
            if live.ws_done() {
                eprintln!("[orderbook_web] Polymarket WebSocket closed; book will stop updating");
                break;
            }
            live.ws_service(100);
            let book = live.ws_copy_orderbook();
            if let Ok(mut g) = poly_book_ws.lock() {
                *g = book;
            }
        }
    });

    let listener =
        TcpListener::bind("127.0.0.1:8080").context("bind to 127.0.0.1:8080 for test web UI")?;
    eprintln!("Serving orderbook test page on http://127.0.0.1:8080 (Poly book updates in background)");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };

        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        let poly_book = poly_book_shared
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| {
                eprintln!("[orderbook_web] mutex poisoned: {e}");
                PolyFullBookPayload::default()
            });
        let cascade_note = cascade_empty_explanation(&kalshi_state, &poly_book);
        let body = render_html(&cfg, &kalshi_state, &poly_book, &cascade_note);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        if let Err(e) = stream.write_all(response.as_bytes()) {
            eprintln!("write error: {e}");
        }
    }

    Ok(())
}

