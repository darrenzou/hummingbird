use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::ArbDb;
use crate::arb_ipc;
use crate::poly_live::{PolyLive, PLACE_ERR_RATE, PLACE_OK, POLY_BATCH_MAX};
use crate::strategy::{self, KalshiState};
use crate::types::*;
use anyhow::Context;
use std::env;
use std::os::unix::io::RawFd;
use std::time::Duration;

const HEDGE_SELL_PRICE: f64 = 0.01;
const HEDGE_BUY_PRICE: f64 = 0.99;
const MAX_HEDGE_RETRIES: usize = 8;
const PRESIGN_COPIES: usize = 5;
const RESIGN_DEBOUNCE_MS: u64 = 2000;

#[derive(Debug, Clone, Default)]
pub struct PolyState {
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub best_bid: f64,
    pub best_ask: f64,
}

#[derive(Debug, Clone)]
pub struct TrackedLevel {
    pub side: Side,
    pub price_cents: i16,
    pub last_sent_vol: f64,
    pub initial_poly_vol: f64,
    pub original_order_qty: u32,
}

#[derive(Debug, Clone)]
struct PreSignedOrder {
    value: serde_json::Value,
    size: u64,
}

struct PreSignedPool {
    sell_levels: Vec<Vec<PreSignedOrder>>,
    buy_levels: Vec<Vec<PreSignedOrder>>,
    max_bit: u32,
}

impl PreSignedPool {
    fn build(live: &PolyLive, total_volume: f64) -> anyhow::Result<Self> {
        let unit = (total_volume / PRESIGN_COPIES as f64).floor().max(1.0) as u64;
        let max_bit = if unit == 0 {
            0
        } else {
            63 - (unit.leading_zeros())
        };

        let mut sell_levels = Vec::new();
        let mut buy_levels = Vec::new();
        let token = &live.token_id;

        for bit in 0..=max_bit {
            let size = 1u64 << bit;
            let mut sells = Vec::new();
            let mut buys = Vec::new();
            for _ in 0..PRESIGN_COPIES {
                let sell_val =
                    live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?;
                sells.push(PreSignedOrder {
                    value: sell_val,
                    size,
                });

                let buy_val =
                    live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?;
                buys.push(PreSignedOrder {
                    value: buy_val,
                    size,
                });
            }
            sell_levels.push(sells);
            buy_levels.push(buys);
        }

        eprintln!(
            "[poly] pre-signed pool: unit={unit} max_bit={max_bit} levels={} orders_per_side={}",
            max_bit + 1,
            (max_bit + 1) as usize * PRESIGN_COPIES
        );

        Ok(PreSignedPool {
            sell_levels,
            buy_levels,
            max_bit,
        })
    }

    fn decompose_and_take(
        &mut self,
        count: u64,
        is_sell: bool,
    ) -> Vec<PreSignedOrder> {
        let levels = if is_sell {
            &mut self.sell_levels
        } else {
            &mut self.buy_levels
        };

        let mut remaining = count;
        let mut orders = Vec::new();

        for bit in (0..=self.max_bit).rev() {
            let size = 1u64 << bit;
            if remaining == 0 {
                break;
            }
            let idx = bit as usize;
            if idx >= levels.len() {
                continue;
            }
            while remaining >= size && !levels[idx].is_empty() {
                orders.push(levels[idx].remove(0));
                remaining -= size;
            }
        }

        if remaining > 0 {
            eprintln!(
                "[poly] pool exhausted, {remaining} remaining unfilled from pre-signed"
            );
        }

        orders
    }

    /// Refill any binary slot that was fully consumed (0 remaining).
    fn refill_exhausted_slots(
        &mut self,
        live: &PolyLive,
        hedge_was_sell_on_poly: bool,
    ) -> anyhow::Result<()> {
        let levels = if hedge_was_sell_on_poly {
            &mut self.sell_levels
        } else {
            &mut self.buy_levels
        };
        let token = &live.token_id;
        for bit in 0..=self.max_bit {
            let idx = bit as usize;
            if idx >= levels.len() {
                continue;
            }
            if !levels[idx].is_empty() {
                continue;
            }
            let size = 1u64 << bit;
            for _ in 0..PRESIGN_COPIES {
                let val = if hedge_was_sell_on_poly {
                    live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?
                } else {
                    live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?
                };
                levels[idx].push(PreSignedOrder { value: val, size });
            }
            eprintln!("[poly] refilled exhausted presign slot bit={bit} size={size}");
        }
        Ok(())
    }

    /// Batch top-up any slot with count < PRESIGN_COPIES on both sides (debounced).
    fn batch_refill_low_slots(&mut self, live: &PolyLive) -> anyhow::Result<()> {
        let token = &live.token_id;
        for bit in 0..=self.max_bit {
            let size = 1u64 << bit;
            let idx = bit as usize;
            if idx < self.sell_levels.len() {
                while self.sell_levels[idx].len() < PRESIGN_COPIES {
                    let val = live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?;
                    self.sell_levels[idx].push(PreSignedOrder { value: val, size });
                }
            }
            if idx < self.buy_levels.len() {
                while self.buy_levels[idx].len() < PRESIGN_COPIES {
                    let val = live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?;
                    self.buy_levels[idx].push(PreSignedOrder { value: val, size });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct PolyContext {
    pub state: PolyState,
    pub tracked_bid: Vec<TrackedLevel>,
    pub tracked_ask: Vec<TrackedLevel>,
    pub kalshi_bids: Vec<PriceLevel>,
    pub kalshi_asks: Vec<PriceLevel>,
}

fn volume_above(st: &PolyState, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    st.bids
        .iter()
        .filter(|l| l.price > thr)
        .map(|l| l.size)
        .sum()
}
fn volume_below(st: &PolyState, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    st.asks
        .iter()
        .filter(|l| l.price < thr)
        .map(|l| l.size)
        .sum()
}

pub fn setup_tracked(ctx: &mut PolyContext, levels: &KalshiLevelsDonePayload) {
    ctx.tracked_bid.clear();
    ctx.tracked_ask.clear();
    for (pc, placed) in levels.bid_levels.iter().take(ARB_MAX_TRACKED) {
        let v = volume_above(&ctx.state, *pc);
        let oq = placed.floor().max(1.0) as u32;
        ctx.tracked_bid.push(TrackedLevel {
            side: Side::Bid,
            price_cents: *pc,
            last_sent_vol: v,
            initial_poly_vol: v,
            original_order_qty: oq,
        });
    }
    for (pc, placed) in levels.ask_levels.iter().take(ARB_MAX_TRACKED) {
        let v = volume_below(&ctx.state, *pc);
        let oq = placed.floor().max(1.0) as u32;
        ctx.tracked_ask.push(TrackedLevel {
            side: Side::Ask,
            price_cents: *pc,
            last_sent_vol: v,
            initial_poly_vol: v,
            original_order_qty: oq,
        });
    }
}

fn rebaseline_tracked(ctx: &mut PolyContext, side: Side) {
    let tracked = if side == Side::Bid {
        &mut ctx.tracked_bid
    } else {
        &mut ctx.tracked_ask
    };
    for t in tracked.iter_mut() {
        t.last_sent_vol = if side == Side::Bid {
            volume_above(&ctx.state, t.price_cents)
        } else {
            volume_below(&ctx.state, t.price_cents)
        };
    }
}

fn threshold_triggers(last: f64, current: f64) -> bool {
    if last > 0.0 {
        let delta = current - last;
        let frac = delta / last;
        if frac < 0.0 && (-frac) > 0.10 {
            return true;
        }
        if frac > 0.15 {
            return true;
        }
    } else if current > 0.0 {
        return true;
    }
    false
}

/// When any tracked level on `side` crosses 10%/15% band, emit `LevelUpdate` for **all** levels on that side.
fn compute_level_updates_ipc(
    ctx: &mut PolyContext,
    side: Side,
    market: &str,
) -> Option<LevelUpdate> {
    let tracked = if side == Side::Bid {
        &mut ctx.tracked_bid
    } else {
        &mut ctx.tracked_ask
    };
    if tracked.is_empty() {
        return None;
    }

    let currents: Vec<f64> = tracked
        .iter()
        .map(|t| {
            if side == Side::Bid {
                volume_above(&ctx.state, t.price_cents)
            } else {
                volume_below(&ctx.state, t.price_cents)
            }
        })
        .collect();

    let mut any = false;
    for (t, &current) in tracked.iter().zip(&currents) {
        if threshold_triggers(t.last_sent_vol, current) {
            any = true;
            break;
        }
    }
    if !any {
        return None;
    }

    let mut updates = Vec::new();
    for (t, current) in tracked.iter_mut().zip(currents) {
        t.last_sent_vol = current;
        let item = if current <= 0.0 || t.initial_poly_vol <= 0.0 {
            LevelUpdateItem {
                level_price_cents: t.price_cents,
                action: LevelAction::Cancel,
                new_qty: None,
            }
        } else {
            let n = (t.original_order_qty as f64 * current / t.initial_poly_vol)
                .floor()
                .max(0.0) as u32;
            if n == 0 {
                LevelUpdateItem {
                    level_price_cents: t.price_cents,
                    action: LevelAction::Cancel,
                    new_qty: None,
                }
            } else {
                LevelUpdateItem {
                    level_price_cents: t.price_cents,
                    action: LevelAction::Amend,
                    new_qty: Some(n),
                }
            }
        };
        updates.push(item);
    }

    Some(LevelUpdate {
        version: IPC_VERSION,
        ts: now_ms(),
        market: market.to_string(),
        updates,
    })
}

#[cfg(test)]
pub fn compute_level_updates(
    ctx: &mut PolyContext,
    side: Side,
) -> Option<PolyLevelVolUpdatePayload> {
    let lu = compute_level_updates_ipc(ctx, side, "")?;
    let levels: Vec<(i16, f64)> = lu
        .updates
        .iter()
        .filter_map(|u| {
            if u.action == LevelAction::Amend {
                Some((u.level_price_cents, u.new_qty.unwrap_or(0) as f64))
            } else {
                Some((u.level_price_cents, 0.0))
            }
        })
        .collect();
    Some(PolyLevelVolUpdatePayload { side, levels })
}

fn send_abort_fatal(fd_out: RawFd, code: &str, message: &str) {
    let _ = arb_ipc::send_msg(
        fd_out,
        &ArbMsg::AbortFatal(AbortFatalPayload {
            version: IPC_VERSION,
            ts: now_ms(),
            reason_code: code.to_string(),
            message: message.to_string(),
        }),
    );
}

fn place_hedge_orders(
    live: &PolyLive,
    fill: &KalshiFillPayload,
    pool: &mut PreSignedPool,
) -> Option<String> {
    let count = fill.filled_count as u64;
    if count == 0 {
        return None;
    }

    let is_sell = fill.side == Side::Bid;
    let mut orders = pool.decompose_and_take(count, is_sell);

    let remaining_from_pool: u64 = orders.iter().map(|o| o.size).sum();

    let shortfall = count.saturating_sub(remaining_from_pool);
    if shortfall > 0 {
        let token = &live.token_id;
        let price = if is_sell {
            HEDGE_SELL_PRICE
        } else {
            HEDGE_BUY_PRICE
        };
        let mut rem = shortfall;
        let mut slot = 1u64;
        while slot * 2 <= rem {
            slot *= 2;
        }
        while rem > 0 && slot > 0 {
            if rem >= slot {
                let val = if is_sell {
                    live.build_signed_sell_order_value(token, price, slot)
                } else {
                    live.build_signed_buy_order_value(token, price, slot)
                };
                match val {
                    Ok(v) => {
                        orders.push(PreSignedOrder { value: v, size: slot });
                        rem -= slot;
                    }
                    Err(e) => {
                        return Some(format!("hedge_sign_fallback_failed:{e}"));
                    }
                }
            }
            slot /= 2;
        }
    }

    let total_size: u64 = orders.iter().map(|o| o.size).sum();
    let values: Vec<serde_json::Value> = orders.iter().map(|o| o.value.clone()).collect();

    for chunk in values.chunks(POLY_BATCH_MAX) {
        let mut placed = false;
        for attempt in 0..MAX_HEDGE_RETRIES {
            let result = live.place_batch_orders(chunk);
            if result == PLACE_OK {
                placed = true;
                break;
            }
            if result == PLACE_ERR_RATE {
                eprintln!(
                    "[poly] hedge batch rate limited, retry {}/{}",
                    attempt + 1,
                    MAX_HEDGE_RETRIES
                );
                std::thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
                continue;
            }
            return Some(format!("hedge_batch_failed:code={result}"));
        }
        if !placed {
            return Some("hedge_rate_limit_exhausted".to_string());
        }
    }

    let _ = pool.refill_exhausted_slots(live, is_sell);

    if total_size < count {
        return Some(format!(
            "hedge_incomplete:placed={total_size},needed={count}"
        ));
    }
    None
}

pub fn poly_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    let creds = ArbCreds::from_env();
    let token = env::var("ARB_TOKEN_ID").context("ARB_TOKEN_ID not set")?;
    let ticker = env::var("ARB_TICKER").unwrap_or_default();
    let market = ticker.clone();
    let neg_risk = matches!(
        env::var("ARB_NEG_RISK").unwrap_or_default().as_str(),
        "1" | "true" | "yes"
    );

    eprintln!(
        "[poly] connecting WS for token={}",
        &token[..16.min(token.len())]
    );
    let mut live = PolyLive::new(&creds, &token, neg_risk);
    live.ws_connect()?;

    let book = live.ws_copy_orderbook();
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
    ctx.state.best_bid = ctx.state.bids.first().map(|l| l.price).unwrap_or(0.0);
    ctx.state.best_ask = ctx.state.asks.first().map(|l| l.price).unwrap_or(1.0);

    eprintln!(
        "[poly] book: {} bids, {} asks | best_bid={:.2} best_ask={:.2}",
        ctx.state.bids.len(),
        ctx.state.asks.len(),
        ctx.state.best_bid,
        ctx.state.best_ask
    );

    let msg: ArbMsg = arb_ipc::recv_msg(fd_in).context("poly recv kalshi snapshot")?;
    let snap = match msg {
        ArbMsg::KalshiBookSnapshot(s) => s,
        ArbMsg::KalshiAbort { reason } => {
            eprintln!("[poly] kalshi abort before snapshot: {reason}");
            return Ok(());
        }
        other => anyhow::bail!("[poly] expected KalshiBookSnapshot, got {other:?}"),
    };

    ctx.kalshi_bids = snap.bids.clone();
    ctx.kalshi_asks = snap.asks.clone();

    let mut st = KalshiState::init();
    st.yes_bids = snap.bids.clone();
    st.yes_asks = snap.asks.clone();
    st.kalshi_balance = env::var("ARB_KALSHI_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000.0);
    st.poly_balance = env::var("ARB_POLY_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| live.get_balance());
    st.side_cap = env::var("ARB_SIDE_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000.0);

    let poly_book = PolyFullBookPayload {
        bids: ctx.state.bids.clone(),
        asks: ctx.state.asks.clone(),
    };

    let cascade = strategy::build_cascade(&st, &poly_book);
    let levels = &cascade.levels;

    let orders_msg = if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        eprintln!("[poly] no cascade levels — sending empty CascadeOrders so Kalshi can unblock");
        CascadeOrders {
            version: IPC_VERSION,
            ts: now_ms(),
            market: market.clone(),
            orders: vec![],
        }
    } else {
        strategy::cascade_result_to_orders(&cascade, &market, now_ms())
    };
    arb_ipc::send_msg(fd_out, &ArbMsg::CascadeOrders(orders_msg)).context("send cascade")?;

    if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        return Ok(());
    }

    let msg2: ArbMsg = arb_ipc::recv_msg(fd_in).context("poly recv levels")?;
    let levels_done = match msg2 {
        ArbMsg::KalshiLevelsDone(l) => l,
        ArbMsg::KalshiAbort { reason } => {
            eprintln!("[poly] kalshi aborted: {reason}");
            if let Ok(db) = ArbDb::open(&creds) {
                let _ = db.record_abort(
                    &market,
                    &format!("kalshi_abort:{reason}"),
                    &ctx.kalshi_bids,
                    &ctx.kalshi_asks,
                    &ctx.state.bids,
                    &ctx.state.asks,
                );
            }
            return Ok(());
        }
        ArbMsg::AbortFatal(a) => {
            eprintln!("[poly] kalshi fatal: {}", a.message);
            return Ok(());
        }
        other => anyhow::bail!("[poly] expected KalshiLevelsDone, got {other:?}"),
    };

    eprintln!(
        "[poly] tracking {} bid levels, {} ask levels",
        levels_done.bid_levels.len(),
        levels_done.ask_levels.len()
    );
    setup_tracked(&mut ctx, &levels_done);

    let total_bid_vol: f64 = levels_done
        .bid_levels
        .iter()
        .map(|(_, v)| v.floor().max(1.0))
        .sum();
    let total_ask_vol: f64 = levels_done
        .ask_levels
        .iter()
        .map(|(_, v)| v.floor().max(1.0))
        .sum();
    let total_volume = total_bid_vol.max(total_ask_vol);

    let mut pool = if total_volume > 0.0 {
        match PreSignedPool::build(&live, total_volume) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[poly] pre-sign pool build failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let db = ArbDb::open(&creds).ok();
    if let Some(db) = &db {
        let _ = db.record_start(
            &market,
            &ctx.kalshi_bids,
            &ctx.kalshi_asks,
            &ctx.state.bids,
            &ctx.state.asks,
        );
    }

    let mut last_trade_ms: u64 = 0;

    loop {
        if live.ws_done() {
            let reason = "poly_ws_disconnected".to_string();
            if let Some(db) = &db {
                let _ = db.record_abort(
                    &market,
                    &reason,
                    &ctx.kalshi_bids,
                    &ctx.kalshi_asks,
                    &ctx.state.bids,
                    &ctx.state.asks,
                );
            }
            arb_ipc::send_msg(fd_out, &ArbMsg::Abort { reason })?;
            break;
        }

        live.ws_service(50);

        let new_book = live.ws_copy_orderbook();
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
        ctx.state.best_bid = ctx.state.bids.first().map(|l| l.price).unwrap_or(0.0);
        ctx.state.best_ask = ctx.state.asks.first().map(|l| l.price).unwrap_or(1.0);

        if last_trade_ms > 0 && now_ms().saturating_sub(last_trade_ms) >= RESIGN_DEBOUNCE_MS {
            if let Some(ref mut p) = pool {
                let _ = p.batch_refill_low_slots(&live);
            }
            last_trade_ms = 0;
        }

        if ctx.state.best_bid > 0.95 || ctx.state.best_ask < 0.05 {
            let reason = format!(
                "price_out_of_band bid={} ask={}",
                ctx.state.best_bid, ctx.state.best_ask
            );
            eprintln!("[poly] {reason}");
            if let Some(db) = &db {
                let _ = db.record_abort(
                    &market,
                    &reason,
                    &ctx.kalshi_bids,
                    &ctx.kalshi_asks,
                    &ctx.state.bids,
                    &ctx.state.asks,
                );
            }
            send_abort_fatal(fd_out, "price_band", &reason);
            break;
        }

        if let Some(u) = compute_level_updates_ipc(&mut ctx, Side::Bid, &market) {
            arb_ipc::send_msg(fd_out, &ArbMsg::LevelUpdate(u))?;
        }
        if let Some(u) = compute_level_updates_ipc(&mut ctx, Side::Ask, &market) {
            arb_ipc::send_msg(fd_out, &ArbMsg::LevelUpdate(u))?;
        }

        if arb_ipc::poll_readable(fd_in, 0).unwrap_or(false) {
            let m: ArbMsg = arb_ipc::recv_msg(fd_in)?;
            match m {
                ArbMsg::KalshiAbort { reason } => {
                    eprintln!("[poly] kalshi abort: {reason}");
                    if let Some(db) = &db {
                        let _ = db.record_abort(
                            &market,
                            &format!("kalshi_abort:{reason}"),
                            &ctx.kalshi_bids,
                            &ctx.kalshi_asks,
                            &ctx.state.bids,
                            &ctx.state.asks,
                        );
                    }
                    break;
                }
                ArbMsg::AbortFatal(a) => {
                    eprintln!("[poly] abort_fatal from kalshi: {}", a.message);
                    if let Some(db) = &db {
                        let _ = db.record_abort(
                            &market,
                            &a.message,
                            &ctx.kalshi_bids,
                            &ctx.kalshi_asks,
                            &ctx.state.bids,
                            &ctx.state.asks,
                        );
                    }
                    break;
                }
                ArbMsg::KalshiBookDelta(d) => {
                    for ch in d.changes {
                        let book = if ch.side == Side::Bid {
                            &mut ctx.kalshi_bids
                        } else {
                            &mut ctx.kalshi_asks
                        };
                        if let Some(lvl) = book.iter_mut().find(|l| (l.price + 0.5) as i16 == ch.price_cents) {
                            lvl.size = ch.new_size;
                        } else if ch.new_size > 0.0 {
                            book.push(PriceLevel {
                                price: ch.price_cents as f64,
                                size: ch.new_size,
                            });
                        }
                    }
                }
                ArbMsg::KalshiFill(fill) => {
                    eprintln!(
                        "[poly] fill: {:?} x{} at {}c oid={}",
                        fill.side,
                        fill.filled_count,
                        fill.price_cents,
                        &fill.order_id[..8.min(fill.order_id.len())]
                    );
                    let hedge_err = if let Some(ref mut p) = pool {
                        place_hedge_orders(&live, &fill, p)
                    } else {
                        let mut fallback =
                            match PreSignedPool::build(&live, fill.filled_count as f64) {
                                Ok(p) => p,
                                Err(e) => {
                                    let reason = format!("hedge_pool_build_failed:{e}");
                                    eprintln!("[poly] {reason}");
                                    if let Some(db) = &db {
                                        let _ = db.record_abort(
                                            &market,
                                            &reason,
                                            &ctx.kalshi_bids,
                                            &ctx.kalshi_asks,
                                            &ctx.state.bids,
                                            &ctx.state.asks,
                                        );
                                    }
                                    send_abort_fatal(fd_out, "pool_build", &reason);
                                    break;
                                }
                            };
                        place_hedge_orders(&live, &fill, &mut fallback)
                    };
                    if let Some(abort_reason) = hedge_err {
                        eprintln!("[poly] hedge failed: {abort_reason}");
                        if let Some(db) = &db {
                            let _ = db.record_abort(
                                &market,
                                &abort_reason,
                                &ctx.kalshi_bids,
                                &ctx.kalshi_asks,
                                &ctx.state.bids,
                                &ctx.state.asks,
                            );
                        }
                        send_abort_fatal(fd_out, "hedge_failed", &abort_reason);
                        break;
                    }
                    last_trade_ms = now_ms();
                    let affected_side = if fill.side == Side::Bid {
                        Side::Bid
                    } else {
                        Side::Ask
                    };
                    live.ws_service(10);
                    let post_book = live.ws_copy_orderbook();
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
                    ctx.state.best_bid = ctx.state.bids.first().map(|l| l.price).unwrap_or(0.0);
                    ctx.state.best_ask = ctx.state.asks.first().map(|l| l.price).unwrap_or(1.0);
                    rebaseline_tracked(&mut ctx, affected_side);

                    if let Some(db) = &db {
                        let _ = db.record_fill(
                            &market,
                            &ctx.kalshi_bids,
                            &ctx.kalshi_asks,
                            &ctx.state.bids,
                            &ctx.state.asks,
                            fill.price_cents as i32,
                            fill.filled_count,
                        );
                    }

                    eprintln!(
                        "[poly] hedge placed for {} x{}, rebaselined {:?} tracked levels",
                        if fill.side == Side::Bid { "SELL" } else { "BUY" },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_book() -> PolyContext {
        let mut ctx = PolyContext::default();
        ctx.state.bids = vec![
            PriceLevel {
                price: 0.60,
                size: 10.0,
            },
            PriceLevel {
                price: 0.59,
                size: 10.0,
            },
            PriceLevel {
                price: 0.40,
                size: 10.0,
            },
        ];
        ctx.state.asks = vec![
            PriceLevel {
                price: 0.41,
                size: 10.0,
            },
            PriceLevel {
                price: 0.50,
                size: 10.0,
            },
            PriceLevel {
                price: 0.90,
                size: 10.0,
            },
        ];
        ctx.state.best_bid = 0.60;
        ctx.state.best_ask = 0.41;
        ctx
    }

    #[test]
    fn sends_update_on_zero_to_positive() {
        let mut ctx = ctx_with_book();
        ctx.tracked_bid = vec![TrackedLevel {
            side: Side::Bid,
            price_cents: 99,
            last_sent_vol: 0.0,
            initial_poly_vol: 1.0,
            original_order_qty: 1,
        }];
        ctx.state
            .bids
            .insert(0, PriceLevel { price: 0.995, size: 2.0 });
        let u = compute_level_updates(&mut ctx, Side::Bid).unwrap();
        assert_eq!(u.levels.len(), 1);
        assert_eq!(u.levels[0].0, 99);
        assert!(u.levels[0].1 > 0.0);
    }

    #[test]
    fn sends_update_on_drop_over_10pct() {
        let mut ctx = ctx_with_book();
        ctx.tracked_bid = vec![TrackedLevel {
            side: Side::Bid,
            price_cents: 50,
            last_sent_vol: 20.0,
            initial_poly_vol: 20.0,
            original_order_qty: 10,
        }];
        ctx.state.bids.retain(|l| l.price != 0.59);
        // Poly volume above 50¢: only 0.60 bid remains → 10 (was 20 with 0.59 included).
        // LevelUpdate carries amended Kalshi qty: floor(10 * 10/20) = 5, not raw poly volume.
        let u = compute_level_updates(&mut ctx, Side::Bid).unwrap();
        assert_eq!(u.levels.len(), 1);
        assert_eq!(u.levels[0].0, 50);
        assert!((u.levels[0].1 - 5.0).abs() < 1e-9);
    }
}
