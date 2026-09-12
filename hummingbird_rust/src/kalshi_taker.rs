//! Kalshi-as-taker path (when Polymarket is the maker).
//!
//! Same IPC as [`crate::arb_poly::poly_taker_process_run`], but hedges with
//! Kalshi fill-or-kill orders instead of a Polymarket pre-sign pool.

use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::{log_db, ArbDb};
use crate::arb_ipc;
use crate::arb_poly::{
    compute_poly_vol_update, handle_kalshi_touch_changed, poly_touch_book_update,
    rebaseline_tracked, setup_tracked, PolyContext,
};
use crate::kalshi_live::{KalshiBatchOrder, KalshiLive, BATCH_MAX};
use crate::poly_live::{format_poly_price_for_tick, round_poly_price};
use crate::shutdown;
use crate::strategy::{self, MakerState};
use crate::types::*;
use anyhow::Context;
use std::env;
use std::os::unix::io::RawFd;
use std::time::Duration;

pub fn kalshi_book_to_taker_payload(kb: &[(f64, f64)], ka: &[(f64, f64)]) -> PolyFullBookPayload {
    let mut bids: Vec<PriceLevel> = kb
        .iter()
        .map(|&(p, s)| PriceLevel {
            price: (p / 100.0).max(0.0),
            size: s,
        })
        .collect();
    let mut asks: Vec<PriceLevel> = ka
        .iter()
        .map(|&(p, s)| PriceLevel {
            price: (p / 100.0).max(0.0),
            size: s,
        })
        .collect();
    bids.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    PolyFullBookPayload { bids, asks }
}

fn kalshi_hedge_signed_fok(live: &KalshiLive, signed_qty: i32) -> Option<String> {
    if signed_qty == 0 {
        return None;
    }
    let mut orders: Vec<KalshiBatchOrder> = Vec::new();
    if signed_qty > 0 {
        let mut rem = signed_qty;
        while rem > 0 {
            let n = rem.min(BATCH_MAX as i32);
            orders.push(KalshiBatchOrder {
                action: "buy".into(),
                count: n,
                yes_price: 99,
                client_order_id: None,
                time_in_force: Some("fill_or_kill".into()),
            });
            rem -= n;
        }
    } else {
        let mut rem = -signed_qty;
        while rem > 0 {
            let n = rem.min(BATCH_MAX as i32);
            orders.push(KalshiBatchOrder {
                action: "sell".into(),
                count: n,
                yes_price: 1,
                client_order_id: None,
                time_in_force: Some("fill_or_kill".into()),
            });
            rem -= n;
        }
    }
    match live.batch_place_orders(&orders) {
        Ok((ids, st)) if (200..300).contains(&st) && ids.iter().any(|s| !s.is_empty()) => None,
        Ok((_, st)) => Some(format!("kalshi hedge HTTP {st}")),
        Err(e) => Some(format!("kalshi hedge {e}")),
    }
}

fn send_abort_fatal(fd_out: RawFd, code: &str, message: &str) {
    arb_ipc::send_msg_eprint(
        fd_out,
        &ArbMsg::AbortFatal(AbortFatalPayload {
            version: IPC_VERSION,
            ts: now_ms(),
            reason_code: code.to_string(),
            message: message.to_string(),
        }),
        "kalshi-taker.AbortFatal",
    );
}

/// Kalshi as taker: stream book, size cascade, FOK-hedge maker fills.
pub fn kalshi_taker_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if let Err(e) = shutdown::install_shutdown_handler() {
        eprintln!("[kalshi-taker] shutdown handler install failed: {e:#}");
    }

    let creds = ArbCreds::from_env();
    let ticker = env::var("ARB_TICKER").context("ARB_TICKER not set")?;
    let market = ticker.clone();

    eprintln!("[kalshi-taker] connecting WS for ticker={ticker}");
    let mut live = KalshiLive::new(&creds, &ticker)?;
    live.ws_connect()?;
    live.seed_orderbook_from_public_rest_if_empty()?;

    let (kb0, ka0) = live.ws_copy_orderbook();
    let book = kalshi_book_to_taker_payload(&kb0, &ka0);

    let mut ctx = PolyContext::default();
    ctx.state.bids = book.bids.clone();
    ctx.state.asks = book.asks.clone();
    ctx.state.bids.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ctx.state.asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ctx.state.best_bid = ctx
        .state
        .bids
        .first()
        .map(|l| round_poly_price(l.price))
        .unwrap_or(0.0);
    ctx.state.best_ask = ctx
        .state
        .asks
        .first()
        .map(|l| round_poly_price(l.price))
        .unwrap_or(1.0);

    const DISP_TICK: f64 = 0.01;
    eprintln!(
        "[kalshi-taker] book: {} bids, {} asks | best_bid={} best_ask={}",
        ctx.state.bids.len(),
        ctx.state.asks.len(),
        format_poly_price_for_tick(ctx.state.best_bid, DISP_TICK),
        format_poly_price_for_tick(ctx.state.best_ask, DISP_TICK),
    );

    let msg: ArbMsg = match arb_ipc::recv_msg(fd_in) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[kalshi-taker] recv MakerBookSnapshot: {e:#}");
            return Err(e).context("poly recv kalshi snapshot");
        }
    };
    let snap = match msg {
        ArbMsg::MakerBookSnapshot(s) => s,
        ArbMsg::MakerAbort { reason } => {
            eprintln!("[kalshi-taker] kalshi abort before snapshot: {reason}");
            return Ok(());
        }
        other => anyhow::bail!("[kalshi-taker] expected MakerBookSnapshot, got {other:?}"),
    };

    ctx.kalshi_bids = snap.bids.clone();
    ctx.kalshi_asks = snap.asks.clone();

    let mut st = MakerState::init();
    st.yes_bids = snap.bids.clone();
    st.yes_asks = snap.asks.clone();
    st.kalshi_balance = env::var("ARB_KALSHI_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| live.get_balance());
    st.poly_balance = env::var("ARB_POLY_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000.0);
    st.side_cap = env::var("ARB_SIDE_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000.0);

    ctx.kalshi_balance = st.kalshi_balance;
    ctx.poly_balance = st.poly_balance;
    ctx.side_cap = st.side_cap;

    let poly_book = PolyFullBookPayload {
        bids: ctx.state.bids.clone(),
        asks: ctx.state.asks.clone(),
    };

    let cascade = strategy::build_cascade(&st, &poly_book);
    let levels = &cascade.levels;

    let orders_msg = if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        eprintln!(
            "[kalshi-taker] no cascade levels — sending empty CascadeOrders so maker can unblock"
        );
        CascadeOrders {
            version: IPC_VERSION,
            ts: now_ms(),
            market: market.clone(),
            orders: vec![],
            taker_book: Some(poly_book.clone()),
        }
    } else {
        strategy::cascade_result_to_orders(&cascade, &market, now_ms(), &poly_book)
    };
    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::CascadeOrders(orders_msg)) {
        eprintln!("[kalshi-taker] send CascadeOrders: {e:#}");
        return Err(e).context("send cascade");
    }

    if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        return Ok(());
    }

    let msg2: ArbMsg = match arb_ipc::recv_msg(fd_in) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[kalshi-taker] recv KalshiLevelsDone: {e:#}");
            return Err(e).context("poly recv levels");
        }
    };
    let levels_done = match msg2 {
        ArbMsg::MakerLevelsDone(l) => l,
        ArbMsg::MakerAbort { reason } => {
            eprintln!("[kalshi-taker] kalshi aborted: {reason}");
            match ArbDb::open(&creds) {
                Ok(db) => {
                    log_db(
                        "poly.record_abort.kalshi_before_levels",
                        db.record_abort(
                            &market,
                            &format!("kalshi_abort:{reason}"),
                            &ctx.kalshi_bids,
                            &ctx.kalshi_asks,
                            &ctx.state.bids,
                            &ctx.state.asks,
                            None,
                        ),
                    );
                }
                Err(e) => eprintln!("[kalshi-taker] ArbDb::open (kalshi abort path): {e:#}"),
            }
            return Ok(());
        }
        ArbMsg::AbortFatal(a) => {
            eprintln!("[kalshi-taker] kalshi fatal: {}", a.message);
            return Ok(());
        }
        other => anyhow::bail!("[kalshi-taker] expected KalshiLevelsDone, got {other:?}"),
    };

    eprintln!(
        "[kalshi-taker] tracking {} bid levels, {} ask levels",
        levels_done.bid_levels.len(),
        levels_done.ask_levels.len()
    );
    setup_tracked(&mut ctx, &levels_done);

    let db = ArbDb::open(&creds).ok();
    if let Some(db) = &db {
        log_db(
            "poly.record_start",
            db.record_start(
                &market,
                &ctx.kalshi_bids,
                &ctx.kalshi_asks,
                &ctx.state.bids,
                &ctx.state.asks,
                None,
            ),
        );
    }

    let mut pending_signed: i32 = 0;
    const POLY_BEST_TOUCH_EPS: f64 = 1e-9;
    let mut last_poly_bb: Option<f64> = None;
    let mut last_poly_ba: Option<f64> = None;

    loop {
        if shutdown::shutdown_requested() {
            eprintln!("[kalshi-taker] shutdown requested");
            if let Some(db) = &db {
                log_db(
                    "poly.record_abort.user_interrupt",
                    db.record_abort(
                        &market,
                        "user_interrupt",
                        &ctx.kalshi_bids,
                        &ctx.kalshi_asks,
                        &ctx.state.bids,
                        &ctx.state.asks,
                        None,
                    ),
                );
            }
            arb_ipc::send_msg_eprint(
                fd_out,
                &ArbMsg::Abort {
                    reason: "user_interrupt".into(),
                },
                "poly.Abort.user_interrupt",
            );
            break;
        }
        if live.ws_done() {
            let reason = "kalshi_ws_disconnected".to_string();
            if let Some(db) = &db {
                log_db(
                    "poly.record_abort.ws_disconnected",
                    db.record_abort(
                        &market,
                        &reason,
                        &ctx.kalshi_bids,
                        &ctx.kalshi_asks,
                        &ctx.state.bids,
                        &ctx.state.asks,
                        None,
                    ),
                );
            }
            if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::Abort { reason }) {
                eprintln!("[kalshi-taker] ws_disconnect Abort IPC send failed: {e:#}");
                return Err(e).context("poly ws_disconnect Abort");
            }
            break;
        }

        live.ws_service(50);

        let (kb_n, ka_n) = live.ws_copy_orderbook();
        let new_book = kalshi_book_to_taker_payload(&kb_n, &ka_n);
        ctx.state.bids = new_book.bids;
        ctx.state.asks = new_book.asks;
        ctx.state.bids.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ctx.state.asks.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ctx.state.best_bid = ctx
            .state
            .bids
            .first()
            .map(|l| round_poly_price(l.price))
            .unwrap_or(0.0);
        ctx.state.best_ask = ctx
            .state
            .asks
            .first()
            .map(|l| round_poly_price(l.price))
            .unwrap_or(1.0);

        if ctx.state.best_bid > 0.95 || ctx.state.best_ask < 0.05 {
            let reason = format!(
                "price_out_of_band bid={} ask={}",
                format_poly_price_for_tick(ctx.state.best_bid, DISP_TICK),
                format_poly_price_for_tick(ctx.state.best_ask, DISP_TICK),
            );
            eprintln!("[kalshi-taker] {reason}");
            if let Some(db) = &db {
                log_db(
                    "poly.record_abort.price_band",
                    db.record_abort(
                        &market,
                        &reason,
                        &ctx.kalshi_bids,
                        &ctx.kalshi_asks,
                        &ctx.state.bids,
                        &ctx.state.asks,
                        None,
                    ),
                );
            }
            send_abort_fatal(fd_out, "price_band", &reason);
            break;
        }

        let bb = ctx.state.best_bid;
        let ba = ctx.state.best_ask;
        if last_poly_bb.is_none() {
            last_poly_bb = Some(bb);
            last_poly_ba = Some(ba);
        } else {
            let lb = last_poly_bb.unwrap();
            let la = last_poly_ba.unwrap();
            let touch_moved =
                (bb - lb).abs() > POLY_BEST_TOUCH_EPS || (ba - la).abs() > POLY_BEST_TOUCH_EPS;
            if touch_moved && (!ctx.tracked_bid.is_empty() || !ctx.tracked_ask.is_empty()) {
                if let Some(u) = poly_touch_book_update(&mut ctx) {
                    eprintln!(
                        "[kalshi-taker] best touch moved bid {}→{} ask {}→{}: PolyLevelVolUpdate (full book) → Kalshi",
                        format_poly_price_for_tick(lb, DISP_TICK),
                        format_poly_price_for_tick(bb, DISP_TICK),
                        format_poly_price_for_tick(la, DISP_TICK),
                        format_poly_price_for_tick(ba, DISP_TICK),
                    );
                    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                        eprintln!(
                            "[kalshi-taker] PolyLevelVolUpdate (touch) IPC send failed: {e:#}"
                        );
                        return Err(e).context("poly PolyLevelVolUpdate touch");
                    }
                }
            }
            last_poly_bb = Some(bb);
            last_poly_ba = Some(ba);
        }

        if let Some(u) = compute_poly_vol_update(&mut ctx, Side::Bid) {
            if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                eprintln!("[kalshi-taker] PolyLevelVolUpdate bid IPC send failed: {e:#}");
                return Err(e).context("poly PolyLevelVolUpdate bid");
            }
        }
        if let Some(u) = compute_poly_vol_update(&mut ctx, Side::Ask) {
            if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                eprintln!("[kalshi-taker] PolyLevelVolUpdate ask IPC send failed: {e:#}");
                return Err(e).context("poly PolyLevelVolUpdate ask");
            }
        }

        let ipc_ready = match arb_ipc::poll_readable(fd_in, 0) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[kalshi-taker] poll_readable: {e:#}");
                false
            }
        };
        if ipc_ready {
            let m: ArbMsg = match arb_ipc::recv_msg(fd_in) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[kalshi-taker] main loop IPC recv: {e:#}");
                    return Err(e).context("poly main loop recv");
                }
            };
            match m {
                ArbMsg::MakerAbort { reason } => {
                    eprintln!("[kalshi-taker] kalshi abort: {reason}");
                    if let Some(db) = &db {
                        log_db(
                            "poly.record_abort.kalshi_ipc",
                            db.record_abort(
                                &market,
                                &format!("kalshi_abort:{reason}"),
                                &ctx.kalshi_bids,
                                &ctx.kalshi_asks,
                                &ctx.state.bids,
                                &ctx.state.asks,
                                None,
                            ),
                        );
                    }
                    break;
                }
                ArbMsg::AbortFatal(a) => {
                    eprintln!("[kalshi-taker] abort_fatal from kalshi: {}", a.message);
                    if let Some(db) = &db {
                        log_db(
                            "poly.record_abort.kalshi_fatal_ipc",
                            db.record_abort(
                                &market,
                                &a.message,
                                &ctx.kalshi_bids,
                                &ctx.kalshi_asks,
                                &ctx.state.bids,
                                &ctx.state.asks,
                                None,
                            ),
                        );
                    }
                    break;
                }
                ArbMsg::MakerTouchChanged(touch) => {
                    handle_kalshi_touch_changed(&mut ctx, &touch, &market, fd_out)?;
                }
                ArbMsg::MakerBookDelta(d) => {
                    for ch in d.changes {
                        let book = if ch.side == Side::Bid {
                            &mut ctx.kalshi_bids
                        } else {
                            &mut ctx.kalshi_asks
                        };
                        if let Some(lvl) = book
                            .iter_mut()
                            .find(|l| (l.price + 0.5) as i16 == ch.price_cents)
                        {
                            lvl.size = ch.new_size;
                        } else if ch.new_size > 0.0 {
                            book.push(PriceLevel {
                                price: ch.price_cents as f64,
                                size: ch.new_size,
                            });
                        }
                    }
                }
                ArbMsg::MakerFill(fill) => {
                    eprintln!(
                        "[kalshi-taker] fill: {:?} x{} at {}c oid={}",
                        fill.side,
                        fill.filled_count,
                        fill.price_cents,
                        &fill.order_id[..8.min(fill.order_id.len())]
                    );
                    strategy::hedge_accum_apply(&mut pending_signed, fill.side, fill.filled_count);
                    let hedge_err = if let Some(q) =
                        crate::taker_runtime::hedge_accum_take_if_over_throttle(
                            &mut pending_signed,
                            crate::taker_runtime::HEDGE_ACCUM_THRESHOLD,
                        ) {
                        kalshi_hedge_signed_fok(&live, q)
                    } else {
                        None
                    };
                    if let Some(abort_reason) = hedge_err {
                        eprintln!("[kalshi-taker] hedge failed: {abort_reason}");
                        if let Some(db) = &db {
                            log_db(
                                "poly.record_abort.hedge_failed",
                                db.record_abort(
                                    &market,
                                    &abort_reason,
                                    &ctx.kalshi_bids,
                                    &ctx.kalshi_asks,
                                    &ctx.state.bids,
                                    &ctx.state.asks,
                                    None,
                                ),
                            );
                        }
                        send_abort_fatal(fd_out, "hedge_failed", &abort_reason);
                        break;
                    }
                    let affected_side = if fill.side == Side::Bid {
                        Side::Bid
                    } else {
                        Side::Ask
                    };
                    live.ws_service(10);
                    let (kb_p, ka_p) = live.ws_copy_orderbook();
                    let post_book = kalshi_book_to_taker_payload(&kb_p, &ka_p);
                    ctx.state.bids = post_book.bids;
                    ctx.state.asks = post_book.asks;
                    ctx.state.bids.sort_by(|a, b| {
                        b.price
                            .partial_cmp(&a.price)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    ctx.state.asks.sort_by(|a, b| {
                        a.price
                            .partial_cmp(&b.price)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    ctx.state.best_bid = ctx
                        .state
                        .bids
                        .first()
                        .map(|l| round_poly_price(l.price))
                        .unwrap_or(0.0);
                    ctx.state.best_ask = ctx
                        .state
                        .asks
                        .first()
                        .map(|l| round_poly_price(l.price))
                        .unwrap_or(1.0);
                    rebaseline_tracked(&mut ctx, affected_side);

                    if let Some(db) = &db {
                        log_db(
                            "poly.record_fill",
                            db.record_fill(
                                &market,
                                &ctx.kalshi_bids,
                                &ctx.kalshi_asks,
                                &ctx.state.bids,
                                &ctx.state.asks,
                                fill.price_cents as i32,
                                fill.filled_count,
                                None,
                            ),
                        );
                    }

                    eprintln!(
                        "[kalshi-taker] fill processed {} x{}, rebaselined {:?} tracked levels",
                        if fill.side == Side::Bid {
                            "SELL"
                        } else {
                            "BUY"
                        },
                        fill.filled_count,
                        affected_side
                    );
                }
                _ => {}
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}
