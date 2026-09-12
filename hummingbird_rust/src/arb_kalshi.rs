use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::{log_db, ArbDb};
use crate::error_policy;
use crate::kalshi_live::{KalshiBatchOrder, KalshiLive, BATCH_MAX};
use crate::maker_runtime::{
    find_level_price, maker_cascade_overlay, maker_run, MakerAmendOps, MakerOrderSlot,
    MakerResizeOutcome, MakerVenueOps,
};
use crate::shutdown;
use crate::types::*;
use anyhow::Context;
use std::env;
use std::os::unix::io::RawFd;
use std::time::Duration;

const MAX_RETRIES: usize = 8;

pub use crate::strategy::{build_cascade, cascade_result_to_orders, CascadeResult, MakerState};

struct KalshiMakerOps {
    live: KalshiLive,
    market: String,
}

impl MakerAmendOps for KalshiMakerOps {
    fn log_tag(&self) -> &'static str {
        "kalshi"
    }
    fn cancel_one(&self, order_id: &str) -> bool {
        self.live.cancel_order(order_id)
    }
    fn cancel_slot(&self, slot: &mut MakerOrderSlot) -> bool {
        let ids: Vec<String> = slot.all_order_ids().cloned().collect();
        if ids.is_empty() {
            return true;
        }
        let mut ok = true;
        for id in ids {
            if !cancel_order_id_best_effort(&self.live, &id) {
                ok = false;
            }
        }
        if ok {
            slot.child_orders.clear();
            slot.order_id.clear();
            slot.last_matched_reported = 0.0;
        }
        ok
    }
    fn resize_slot(
        &self,
        slot: &mut MakerOrderSlot,
        action: &str,
        new_count: i32,
    ) -> MakerResizeOutcome {
        if new_count == slot.current_count {
            return MakerResizeOutcome::Ok;
        }
        if self.live.amend_order(
            &slot.order_id,
            "yes",
            action,
            slot.yes_price_cents as i32,
            new_count,
        ) {
            slot.current_count = new_count;
            MakerResizeOutcome::Ok
        } else {
            MakerResizeOutcome::FailedNoChange
        }
    }
}

impl MakerVenueOps for KalshiMakerOps {
    fn market(&self) -> &str {
        &self.market
    }
    fn snapshot_book_for_ipc(&mut self) -> MakerBookSnapshot {
        let (bids_raw, asks_raw) = self.live.ws_copy_orderbook();
        MakerBookSnapshot {
            version: IPC_VERSION,
            market: self.market.clone(),
            ts: now_ms(),
            bids: bids_raw
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect(),
            asks: asks_raw
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect(),
        }
    }
    fn ws_book_cents(&mut self) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        self.live.ws_copy_orderbook()
    }
    fn ws_book_as_yes_levels(&mut self) -> (Vec<PriceLevel>, Vec<PriceLevel>) {
        let (b, a) = self.live.ws_copy_orderbook();
        let kb: Vec<PriceLevel> = b
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        let ka: Vec<PriceLevel> = a
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        (kb, ka)
    }
    fn ws_service(&mut self, ms: u64) {
        self.live.ws_service(ms);
    }
    fn ws_done(&self) -> bool {
        self.live.ws_done()
    }
    fn subscribe_fills(&mut self) {
        self.live.ws_subscribe_fills();
    }
    fn batch_place_initial(
        &mut self,
        orders: &[CascadeOrder],
        taker_book: &PolyFullBookPayload,
        db: &Option<ArbDb>,
    ) -> anyhow::Result<Vec<String>> {
        let poly_at_cascade = taker_book;
        let bid_batch: Vec<KalshiBatchOrder> = orders
            .iter()
            .filter(|o| o.side == Side::Bid)
            .map(|o| KalshiBatchOrder {
                action: "buy".to_string(),
                count: o.qty as i32,
                yes_price: o.limit_price_cents as i32,
                client_order_id: None,
                time_in_force: None,
            })
            .collect();
        let ask_batch: Vec<KalshiBatchOrder> = orders
            .iter()
            .filter(|o| o.side == Side::Ask)
            .map(|o| KalshiBatchOrder {
                action: "sell".to_string(),
                count: o.qty as i32,
                yes_price: o.limit_price_cents as i32,
                client_order_id: None,
                time_in_force: None,
            })
            .collect();
        let (k_bids, k_asks) = self.live.ws_copy_orderbook();
        let kb_now: Vec<PriceLevel> = k_bids
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        let ka_now: Vec<PriceLevel> = k_asks
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        let mut bid_ids: Vec<String> = Vec::new();
        if !bid_batch.is_empty() {
            for chunk in bid_batch.chunks(BATCH_MAX) {
                match batch_place_with_retry(
                    &self.live,
                    chunk,
                    db,
                    &self.market,
                    &kb_now,
                    &ka_now,
                    &poly_at_cascade.bids,
                    &poly_at_cascade.asks,
                ) {
                    Ok(ids) => bid_ids.extend(ids),
                    Err(e) => {
                        eprintln!("[kalshi] bid batch failed: {e}");
                        cancel_all_placed_ids(&self.live, &bid_ids, &[]);
                        if let Some(d) = db.as_ref() {
                            log_db(
                                "kalshi.record_error.bid_place_failed",
                                d.record_error(
                                    &self.market,
                                    "kalshi",
                                    "place_failed",
                                    &e.to_string(),
                                    None,
                                    false,
                                    &kb_now,
                                    &ka_now,
                                    &poly_at_cascade.bids,
                                    &poly_at_cascade.asks,
                                    &serde_json::json!({}),
                                    None,
                                ),
                            );
                        }
                        return Err(e);
                    }
                }
            }
        }
        let mut ask_ids: Vec<String> = Vec::new();
        if !ask_batch.is_empty() {
            for chunk in ask_batch.chunks(BATCH_MAX) {
                match batch_place_with_retry(
                    &self.live,
                    chunk,
                    db,
                    &self.market,
                    &kb_now,
                    &ka_now,
                    &poly_at_cascade.bids,
                    &poly_at_cascade.asks,
                ) {
                    Ok(ids) => ask_ids.extend(ids),
                    Err(e) => {
                        eprintln!("[kalshi] ask batch failed: {e}");
                        cancel_all_placed_ids(&self.live, &bid_ids, &ask_ids);
                        if let Some(d) = db.as_ref() {
                            log_db(
                                "kalshi.record_error.ask_place_failed",
                                d.record_error(
                                    &self.market,
                                    "kalshi",
                                    "place_failed",
                                    &e.to_string(),
                                    None,
                                    false,
                                    &kb_now,
                                    &ka_now,
                                    &poly_at_cascade.bids,
                                    &poly_at_cascade.asks,
                                    &serde_json::json!({}),
                                    None,
                                ),
                            );
                        }
                        return Err(e);
                    }
                }
            }
        }
        Ok(merge_order_ids(orders, &bid_ids, &ask_ids))
    }

    fn touch_cascade_pivot(
        &mut self,
        pivot: &TouchCascadePivot,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        db: &Option<ArbDb>,
    ) -> anyhow::Result<()> {
        kalshi_touch_cascade_pivot_impl(
            &mut self.live,
            db,
            &self.market,
            pivot,
            bid_slots,
            ask_slots,
        )
    }

    fn poll_fill_events(
        &mut self,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        db: &Option<ArbDb>,
    ) -> Vec<MakerFillPayload> {
        let mut out = Vec::new();
        while let Some((count, is_bid, oid)) = self.live.ws_poll_fill() {
            let slots_ref = if is_bid {
                bid_slots.as_slice()
            } else {
                ask_slots.as_slice()
            };
            let (level_pc, yes_pc) = find_level_price(slots_ref, &oid);
            let price_cents = if yes_pc > 0 { yes_pc } else { level_pc };
            let slots = if is_bid {
                bid_slots.as_mut_slice()
            } else {
                ask_slots.as_mut_slice()
            };
            if let Some(slot) = slots
                .iter_mut()
                .find(|s| s.order_id == oid || s.child_orders.iter().any(|c| c.order_id == oid))
            {
                slot.current_count = (slot.current_count - count as i32).max(0);
            }
            let (k_bids, k_asks) = self.live.ws_copy_orderbook();
            let kb: Vec<PriceLevel> = k_bids
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            let ka: Vec<PriceLevel> = k_asks
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            if let Some(db) = db {
                let cascade = maker_cascade_overlay(bid_slots, ask_slots);
                log_db(
                    "kalshi.record_fill",
                    db.record_fill(
                        &self.market,
                        &kb,
                        &ka,
                        &[],
                        &[],
                        price_cents as i32,
                        count,
                        Some(&cascade),
                    ),
                );
            }
            out.push(MakerFillPayload {
                ts: now_ms(),
                order_id: oid.clone(),
                side: if is_bid { Side::Bid } else { Side::Ask },
                price_cents,
                filled_count: count,
                market: self.market.clone(),
            });
        }
        out
    }

    fn recalc_balance_kalshi_leg(&mut self) -> f64 {
        self.live.get_balance()
    }
    fn recalc_balance_poly_leg(&mut self) -> f64 {
        2000.0
    }
    fn db_log_tag(&self) -> &'static str {
        "kalshi"
    }
    fn cancel_every_slot_best_effort(
        &mut self,
        bid_slots: &mut [MakerOrderSlot],
        ask_slots: &mut [MakerOrderSlot],
    ) {
        cancel_all_cascade_slots(&self.live, bid_slots, ask_slots);
    }
    fn books_for_abort_log(
        &mut self,
    ) -> (
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
    ) {
        let (k_bids, k_asks) = self.live.ws_copy_orderbook();
        let kb: Vec<PriceLevel> = k_bids
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        let ka: Vec<PriceLevel> = k_asks
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        (kb, ka, vec![], vec![])
    }
}

fn cancel_order_id_best_effort(live: &KalshiLive, order_id: &str) -> bool {
    if order_id.is_empty() {
        return true;
    }
    if live.cancel_order(order_id) {
        return true;
    }
    std::thread::sleep(Duration::from_millis(300));
    live.cancel_order(order_id)
}

/// Cancel IDs returned from batch place (partial progress on multi-chunk batches).
fn cancel_all_placed_ids(live: &KalshiLive, bid_ids: &[String], ask_ids: &[String]) {
    for oid in bid_ids.iter().chain(ask_ids.iter()) {
        if !oid.is_empty() {
            let _ = cancel_order_id_best_effort(live, oid);
        }
    }
}

/// Cancel every known cascade order (by id), even if local `current_count` is stale.
fn cancel_all_cascade_slots(
    live: &KalshiLive,
    bid_slots: &[MakerOrderSlot],
    ask_slots: &[MakerOrderSlot],
) {
    let n = bid_slots
        .iter()
        .chain(ask_slots.iter())
        .map(|s| s.all_order_ids().count())
        .sum::<usize>();
    if n == 0 {
        return;
    }
    eprintln!("[kalshi] exit: cancelling {n} cascade order id(s)…");
    for slot in bid_slots.iter().chain(ask_slots.iter()) {
        for oid in slot.all_order_ids() {
            if cancel_order_id_best_effort(live, oid) {
                eprintln!(
                    "[kalshi] cancelled cascade level {}¢ (limit {}¢) oid={}…",
                    slot.level_price_cents,
                    slot.yes_price_cents,
                    &oid[..8.min(oid.len())]
                );
            } else {
                eprintln!(
                    "[kalshi] cascade cancel FAILED level {}¢ oid={}… — may still rest on Kalshi",
                    slot.level_price_cents,
                    &oid[..8.min(oid.len())]
                );
            }
        }
    }
}

fn batch_place_with_retry(
    live: &KalshiLive,
    orders: &[KalshiBatchOrder],
    db: &Option<ArbDb>,
    market: &str,
    kb: &[PriceLevel],
    ka: &[PriceLevel],
    pb: &[PriceLevel],
    pa: &[PriceLevel],
) -> anyhow::Result<Vec<String>> {
    let mut attempt = 0usize;
    loop {
        let (ids, status) = live.batch_place_orders(orders)?;
        if error_policy::http_is_rate_limited(status) {
            if let Some(db) = db {
                log_db(
                    "kalshi.record_error.batch_rate_limit",
                    db.record_error(
                        market,
                        "kalshi",
                        "rate_limit",
                        &format!("batch_place HTTP {status}"),
                        Some(status),
                        true,
                        kb,
                        ka,
                        pb,
                        pa,
                        &serde_json::json!({}),
                        None,
                    ),
                );
            }
            if attempt < MAX_RETRIES {
                let backoff = 500u64 * (1u64 << attempt.min(6));
                eprintln!(
                    "[kalshi] rate limited, retry {}/{} in {}ms",
                    attempt + 1,
                    MAX_RETRIES,
                    backoff
                );
                std::thread::sleep(Duration::from_millis(backoff));
                attempt += 1;
                continue;
            }
            anyhow::bail!("batch place: rate limited after {MAX_RETRIES} retries");
        }
        if status == 0 {
            if attempt < MAX_RETRIES {
                std::thread::sleep(Duration::from_millis(300 * (attempt as u64 + 1)));
                attempt += 1;
                continue;
            }
            anyhow::bail!("batch place: network error");
        }
        if status >= 400 {
            anyhow::bail!("batch place: HTTP {status}");
        }
        if ids.len() != orders.len() {
            anyhow::bail!(
                "batch place: response had {} order entries for {} submitted (check nested order.order_id parsing)",
                ids.len(),
                orders.len()
            );
        }
        if let Some(i) = ids.iter().position(|s| s.is_empty()) {
            anyhow::bail!(
                "batch place: empty order_id at response index {i} (Kalshi may have returned per-order errors)"
            );
        }
        return Ok(ids);
    }
}

fn merge_order_ids(orders: &[CascadeOrder], bid_ids: &[String], ask_ids: &[String]) -> Vec<String> {
    let mut bi = 0usize;
    let mut ai = 0usize;
    let mut out = Vec::with_capacity(orders.len());
    for o in orders {
        if o.side == Side::Bid {
            if bi < bid_ids.len() {
                out.push(bid_ids[bi].clone());
                bi += 1;
            } else {
                out.push(String::new());
            }
        } else if ai < ask_ids.len() {
            out.push(ask_ids[ai].clone());
            ai += 1;
        } else {
            out.push(String::new());
        }
    }
    out
}

pub fn kalshi_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if crate::arb_config::env_maker_is_polymarket() {
        return crate::kalshi_taker::kalshi_taker_process_run(fd_in, fd_out);
    }
    kalshi_maker_process_run(fd_in, fd_out)
}

pub fn kalshi_maker_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if let Err(e) = shutdown::install_shutdown_handler() {
        eprintln!("[kalshi] shutdown handler install failed: {e:#}");
    }

    let creds = ArbCreds::from_env();
    let ticker = env::var("ARB_TICKER").context("ARB_TICKER not set")?;
    eprintln!("[kalshi] connecting WS for ticker={ticker}");
    let mut live = KalshiLive::new(&creds, &ticker)?;
    live.ws_connect()?;
    live.seed_orderbook_from_public_rest_if_empty()?;

    let (bids_raw, asks_raw) = live.ws_copy_orderbook();
    eprintln!(
        "[kalshi] orderbook: {} bids, {} asks",
        bids_raw.len(),
        asks_raw.len()
    );

    let market = ticker.clone();
    let mut ops = KalshiMakerOps { live, market };
    maker_run(&mut ops, fd_in, fd_out, &creds)
}

fn kalshi_touch_cascade_pivot_impl(
    live: &mut KalshiLive,
    db: &Option<ArbDb>,
    market: &str,
    pivot: &TouchCascadePivot,
    bid_slots: &mut Vec<MakerOrderSlot>,
    ask_slots: &mut Vec<MakerOrderSlot>,
) -> anyhow::Result<()> {
    let slots = match pivot.side {
        Side::Bid => &mut *bid_slots,
        Side::Ask => &mut *ask_slots,
    };
    let Some(idx) = slots.iter().position(|s| {
        s.level_price_cents == pivot.drop_level_price_cents && !s.order_id.is_empty()
    }) else {
        eprintln!(
            "[kalshi] touch pivot: no open slot at drop level {}¢",
            pivot.drop_level_price_cents
        );
        return Ok(());
    };
    let old = slots.remove(idx);
    for id in old.all_order_ids() {
        let _ = cancel_order_id_best_effort(live, id);
    }

    let action = if pivot.new_order.side == Side::Bid {
        "buy"
    } else {
        "sell"
    };
    let chunk = [KalshiBatchOrder {
        action: action.to_string(),
        count: pivot.new_order.qty as i32,
        yes_price: pivot.new_order.limit_price_cents as i32,
        client_order_id: None,
        time_in_force: None,
    }];

    let (k_bids, k_asks) = live.ws_copy_orderbook();
    let kb: Vec<PriceLevel> = k_bids
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    let ka: Vec<PriceLevel> = k_asks
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    let poly_empty = PolyFullBookPayload::default();

    let ids = batch_place_with_retry(
        live,
        &chunk,
        db,
        market,
        &kb,
        &ka,
        &poly_empty.bids,
        &poly_empty.asks,
    )?;
    let oid = ids.get(0).cloned().unwrap_or_default();
    if oid.is_empty() {
        anyhow::bail!("touch pivot: empty order id from batch place");
    }

    slots.push(MakerOrderSlot {
        order_id: oid,
        level_price_cents: pivot.new_order.level_price_cents,
        yes_price_cents: pivot.new_order.limit_price_cents,
        original_count: pivot.new_order.qty as i32,
        current_count: pivot.new_order.qty as i32,
        initial_taker_vol: pivot.new_order.initial_taker_vol,
        initial_maker_vol: pivot.new_order.initial_maker_vol,
        child_orders: Vec::new(),
        last_matched_reported: 0.0,
    });

    if pivot.side == Side::Bid {
        slots.sort_by(|a, b| b.level_price_cents.cmp(&a.level_price_cents));
    } else {
        slots.sort_by(|a, b| a.level_price_cents.cmp(&b.level_price_cents));
    }

    eprintln!(
        "[kalshi] touch pivot {:?}: dropped {}¢, placed {}¢ x{}",
        pivot.side,
        pivot.drop_level_price_cents,
        pivot.new_order.limit_price_cents,
        pivot.new_order.qty
    );
    Ok(())
}
