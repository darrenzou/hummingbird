//! Local diagnostic page that renders both books and the current cascade.
//!
//! Historical helper (`cargo run --bin orderbook_web`). Not part of the live bot.

use anyhow::Context;
use hummingbird_rust::arb_config::{self, ArbConfig, MakerVenue};
use hummingbird_rust::kalshi_live::KalshiLive;
use hummingbird_rust::kalshi_taker::kalshi_book_to_taker_payload;
use hummingbird_rust::poly_live::{
    format_poly_price_for_tick, round_poly_price, PolyClobConstraints, PolyLive,
};
use hummingbird_rust::strategy::{
    build_cascade, cascade_ask_limit_price, cascade_bid_limit_price, cascade_empty_explanation,
    MakerState,
};
use hummingbird_rust::types::{PolyFullBookPayload, PriceLevel};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

fn render_html(
    cfg: &ArbConfig,
    maker_st: &MakerState,
    taker_book: &PolyFullBookPayload,
    cascade_note: &str,
    poly_tick: f64,
    maker_venue: MakerVenue,
) -> String {
    let mut html = String::new();
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Orderbooks + Cascade</title>",
    );
    html.push_str(
        "<style>\
body{font-family:Arial,Helvetica,sans-serif;margin:20px;}\
.tables-row{display:flex;flex-wrap:wrap;gap:20px;align-items:flex-start;margin:16px 0;}\
.table-panel{flex:1 1 280px;min-width:240px;max-width:100%;}\
.table-panel h2{margin:0 0 10px 0;font-size:1.1em;}\
table{border-collapse:collapse;width:100%;margin:0;}\
th,td{border:1px solid #ccc;padding:4px 8px;text-align:right;font-size:13px;}\
th{text-align:center;background:#f5f5f5;}\
h2{margin-top:24px;}\
</style>",
    );
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
        let m = match pair0.maker {
            MakerVenue::Kalshi => "kalshi",
            MakerVenue::Polymarket => "polymarket",
        };
        html.push_str(&format!(
            "<li>pair: kalshi_ticker={} polymarket_token_id={} neg_risk={} maker={}</li>",
            pair0.kalshi_ticker, pair0.polymarket_token_id, pair0.neg_risk, m
        ));
    }
    html.push_str("</ul>");

    let (maker_title, taker_title) = match maker_venue {
        MakerVenue::Kalshi => (
            "Maker — Kalshi (REST snapshot)",
            "Taker — Polymarket (WebSocket)",
        ),
        MakerVenue::Polymarket => (
            "Maker — Polymarket (WebSocket)",
            "Taker — Kalshi (REST snapshot)",
        ),
    };

    // Maker / taker books side by side.
    html.push_str("<div class=\"tables-row\">");

    html.push_str(&format!(
        "<div class=\"table-panel\"><h2>{maker_title}</h2>"
    ));
    html.push_str(
        "<table><tr><th colspan=\"2\">Yes Bids</th><th></th><th colspan=\"2\">Yes Asks</th></tr>",
    );
    html.push_str(
        "<tr><th>Price (¢)</th><th>Size</th><th></th><th>Price (¢)</th><th>Size</th></tr>",
    );
    let rows = maker_st.yes_bids.len().max(maker_st.yes_asks.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some(b) = maker_st.yes_bids.get(i) {
            html.push_str(&format!("<td>{:.0}</td><td>{:.2}</td>", b.price, b.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some(a) = maker_st.yes_asks.get(i) {
            html.push_str(&format!("<td>{:.0}</td><td>{:.2}</td>", a.price, a.size));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div>");

    html.push_str(&format!(
        "<div class=\"table-panel\"><h2>{taker_title}</h2>"
    ));
    html.push_str("<p style=\"margin:0 0 8px 0;font-size:12px;color:#444;\">Refresh the page for updates. Bids: highest at top. Asks: lowest (best ask) at top.</p>");
    html.push_str(
        "<table><tr><th colspan=\"2\">Bids</th><th></th><th colspan=\"2\">Asks</th></tr>",
    );
    html.push_str(
        "<tr><th>Price ($)</th><th>Size</th><th></th><th>Price ($)</th><th>Size</th></tr>",
    );
    let rows = taker_book.bids.len().max(taker_book.asks.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some(b) = taker_book.bids.get(i) {
            html.push_str(&format!(
                "<td>{}</td><td>{:.2}</td>",
                format_poly_price_for_tick(b.price, poly_tick),
                b.size
            ));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some(a) = taker_book.asks.get(i) {
            html.push_str(&format!(
                "<td>{}</td><td>{:.2}</td>",
                format_poly_price_for_tick(a.price, poly_tick),
                a.size
            ));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div>");

    // Cascade levels (same rules as arb: Poly book + Kalshi ladder + portfolio cap).
    // Cascade price = limit (¢) the live bot submits — uses same helpers as cascade_result_to_orders.
    let cascade = build_cascade(maker_st, taker_book);
    let levels = &cascade.levels;
    html.push_str("<div class=\"table-panel\"><h2>Cascade (build_cascade)</h2>");
    html.push_str("<p style=\"margin:0 0 8px 0;font-size:12px;color:#444;\">Price = limit (¢) at which live bot posts resting orders on the <strong>maker</strong> venue.</p>");
    html.push_str(
        "<table><tr><th colspan=\"2\">Bids</th><th></th><th colspan=\"2\">Asks</th></tr>",
    );
    html.push_str("<tr><th>Cascade price (¢)</th><th>Volume</th><th></th><th>Cascade price (¢)</th><th>Volume</th></tr>");
    let rows = levels.bid_levels.len().max(levels.ask_levels.len());
    for i in 0..rows {
        html.push_str("<tr>");
        if let Some((k_bid, v)) = levels.bid_levels.get(i) {
            let cascade_price = cascade_bid_limit_price(*k_bid);
            html.push_str(&format!("<td>{}</td><td>{:.2}</td>", cascade_price, v));
        } else {
            html.push_str("<td></td><td></td>");
        }
        html.push_str("<td></td>");
        if let Some((level_cents, v)) = levels.ask_levels.get(i) {
            let cascade_price = cascade_ask_limit_price(*level_cents);
            html.push_str(&format!("<td>{}</td><td>{:.2}</td>", cascade_price, v));
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

    let mut kalshi_state = MakerState::init();
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
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    kalshi_state.yes_asks = asks_raw
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    kalshi_state.yes_bids.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    kalshi_state.yes_asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Polymarket: WS snapshot + deltas on a dedicated thread. **Do not** wrap `PolyLive` in the same
    // mutex HTTP uses: `ws_service` can run a long time while draining many messages, which would
    // block the browser on `lock()` forever. Publish only `PolyFullBookPayload` with a short lock.
    let poly_clob = PolyClobConstraints::fetch_for_clob_token(&pair0.polymarket_token_id)
        .unwrap_or_else(|e| {
            eprintln!(
                "[orderbook_web] Gamma constraints failed ({e:#}); using config neg_risk={}",
                pair0.neg_risk
            );
            PolyClobConstraints::legacy_from_config(pair0.neg_risk)
        });
    let poly_tick = poly_clob.tick;
    let mut poly_live = PolyLive::new(&creds, &pair0.polymarket_token_id, poly_clob);
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
    eprintln!(
        "Serving orderbook test page on http://127.0.0.1:8080 (Poly book updates in background)"
    );

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

        if pair0.maker == MakerVenue::Polymarket {
            let tb = if let Ok((br, ar)) = KalshiLive::fetch_public_orderbook(&pair0.kalshi_ticker)
            {
                kalshi_book_to_taker_payload(&br, &ar)
            } else {
                PolyFullBookPayload::default()
            };
            let mut m = MakerState::init();
            m.kalshi_balance = kalshi_state.kalshi_balance;
            m.poly_balance = kalshi_state.poly_balance;
            m.side_cap = kalshi_state.side_cap;
            m.yes_bids = poly_book
                .bids
                .iter()
                .map(|l| PriceLevel {
                    price: round_poly_price(l.price) * 100.0,
                    size: l.size,
                })
                .collect();
            m.yes_asks = poly_book
                .asks
                .iter()
                .map(|l| PriceLevel {
                    price: round_poly_price(l.price) * 100.0,
                    size: l.size,
                })
                .collect();
            m.yes_bids.sort_by(|a, b| {
                b.price
                    .partial_cmp(&a.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            m.yes_asks.sort_by(|a, b| {
                a.price
                    .partial_cmp(&b.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let m = Box::leak(Box::new(m));
            let t = Box::leak(Box::new(tb));
            let cascade_note = cascade_empty_explanation(m, t);
            let body = render_html(&cfg, m, t, &cascade_note, poly_tick, MakerVenue::Polymarket);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            if let Err(e) = stream.write_all(response.as_bytes()) {
                eprintln!("write error: {e}");
            }
            continue;
        }

        if let Ok((bids_raw, asks_raw)) = KalshiLive::fetch_public_orderbook(&pair0.kalshi_ticker) {
            kalshi_state.yes_bids = bids_raw
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            kalshi_state.yes_asks = asks_raw
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            kalshi_state.yes_bids.sort_by(|a, b| {
                b.price
                    .partial_cmp(&a.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            kalshi_state.yes_asks.sort_by(|a, b| {
                a.price
                    .partial_cmp(&b.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        let cascade_note = cascade_empty_explanation(&kalshi_state, &poly_book);
        let body = render_html(
            &cfg,
            &kalshi_state,
            &poly_book,
            &cascade_note,
            poly_tick,
            MakerVenue::Kalshi,
        );
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
