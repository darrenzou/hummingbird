use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::{log_db, ArbDb};
use crate::arb_ipc;
use crate::maker_runtime::{
    k_cent_tuples_to_price_levels, maker_cascade_overlay, maker_run, ChildOrder, MakerAmendOps,
    MakerOrderSlot, MakerResizeOutcome, MakerVenueOps,
};
use crate::poly_live::{
    ceil_price_to_tick, decompose_order_sizes, floor_price_to_tick, format_poly_price_for_tick,
    round_poly_price, PolyClobConstraints, PolyLive, PLACE_ERR_RATE, PLACE_OK, POLY_BATCH_MAX,
};
use crate::shutdown;
use crate::strategy::{self, MakerState};
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

fn poly_ws_book_to_cent_tuples(book: &PolyFullBookPayload) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let mut bids: Vec<(f64, f64)> = book
        .bids
        .iter()
        .map(|l| (round_poly_price(l.price) * 100.0, l.size))
        .collect();
    let mut asks: Vec<(f64, f64)> = book
        .asks
        .iter()
        .map(|l| (round_poly_price(l.price) * 100.0, l.size))
        .collect();
    bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    (bids, asks)
}

fn poly_ws_book_to_maker_cent_price_levels(
    book: &PolyFullBookPayload,
) -> (Vec<PriceLevel>, Vec<PriceLevel>) {
    let mut bids: Vec<PriceLevel> = book
        .bids
        .iter()
        .map(|l| PriceLevel {
            price: round_poly_price(l.price) * 100.0,
            size: l.size,
        })
        .collect();
    let mut asks: Vec<PriceLevel> = book
        .asks
        .iter()
        .map(|l| PriceLevel {
            price: round_poly_price(l.price) * 100.0,
            size: l.size,
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
    (bids, asks)
}

fn poly_cancel_all_cascade_slots(
    live: &PolyLive,
    bid_slots: &[MakerOrderSlot],
    ask_slots: &[MakerOrderSlot],
) {
    let n: usize = bid_slots
        .iter()
        .chain(ask_slots.iter())
        .map(|s| s.all_order_ids().count())
        .sum();
    if n == 0 {
        return;
    }
    eprintln!("[poly-maker] exit: cancelling {n} cascade order id(s)…");
    for slot in bid_slots.iter().chain(ask_slots.iter()) {
        for oid in slot.all_order_ids() {
            if live.cancel_clob_order(oid) {
                eprintln!(
                    "[poly-maker] cancelled cascade level {}¢ (limit {}¢) oid={}…",
                    slot.level_price_cents,
                    slot.yes_price_cents,
                    &oid[..8.min(oid.len())]
                );
            } else {
                eprintln!(
                    "[poly-maker] cascade cancel FAILED level {}¢ oid={}…",
                    slot.level_price_cents,
                    &oid[..8.min(oid.len())]
                );
            }
        }
    }
}

fn poly_handle_touch_cascade_pivot(
    live: &PolyLive,
    token: &str,
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
            "[poly-maker] touch pivot: no open slot at drop level {}¢",
            pivot.drop_level_price_cents
        );
        return Ok(());
    };
    let old = slots.remove(idx);
    for oid in old.all_order_ids() {
        let _ = live.cancel_clob_order(oid);
    }

    let px = (pivot.new_order.limit_price_cents as f64) / 100.0;
    let px = if pivot.new_order.side == Side::Bid {
        floor_price_to_tick(px, live.constraints.tick)
    } else {
        ceil_price_to_tick(px, live.constraints.tick)
    };
    let js = if pivot.new_order.side == Side::Bid {
        live.build_signed_buy_limit_json(token, px, pivot.new_order.qty as u64, "GTC")?
    } else {
        live.build_signed_sell_limit_json(token, px, pivot.new_order.qty as u64, "GTC")?
    };
    let v: serde_json::Value = serde_json::from_str(&js)?;
    let ids = live.place_batch_orders_ids(&[v])?;
    let oid = ids.get(0).cloned().unwrap_or_default();
    if oid.is_empty() {
        anyhow::bail!("poly touch pivot: empty order id");
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
        "[poly-maker] touch pivot {:?}: dropped {}¢, placed {}¢ x{}",
        pivot.side,
        pivot.drop_level_price_cents,
        pivot.new_order.limit_price_cents,
        pivot.new_order.qty
    );
    Ok(())
}

/// Open-size **increase** delta must be at least Polymarket `order_min_size` (contracts).
#[inline]
pub fn poly_increase_delta_meets_min_size(delta: i32, order_min_size: u64) -> bool {
    (delta as f64) >= order_min_size.max(1) as f64
}

/// After garbage-collecting fully filled child orders, realign `last_matched_reported` with
/// summed API `matched` so later primary fills are not suppressed.
fn poly_sync_slot_last_matched_reported(slot: &mut MakerOrderSlot, primary_matched: f64) {
    if slot.order_id.is_empty() {
        slot.last_matched_reported = 0.0;
        return;
    }
    slot.last_matched_reported = primary_matched
        + slot
            .child_orders
            .iter()
            .map(|c| c.last_matched)
            .sum::<f64>();
}

fn poly_primary_matched_size_or_skip(result: anyhow::Result<f64>, order_id: &str) -> Option<f64> {
    match result {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("[poly-maker] matched-size read failed for primary {order_id}: {e:#}");
            None
        }
    }
}

struct PolyMakerOps {
    live: PolyLive,
    market: String,
    token: String,
}

impl PolyMakerOps {
    fn sign_place_one(&self, yes_price_cents: i16, action: &str, count: i32) -> Result<String, ()> {
        if count <= 0 {
            return Err(());
        }
        let px = (yes_price_cents as f64) / 100.0;
        let px = if action == "buy" {
            floor_price_to_tick(px, self.live.constraints.tick)
        } else {
            ceil_price_to_tick(px, self.live.constraints.tick)
        };
        let tok = self.token.as_str();
        let js = if action == "buy" {
            match self
                .live
                .build_signed_buy_limit_json(tok, px, count as u64, "GTC")
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[poly-maker] buy sign: {e:#}");
                    return Err(());
                }
            }
        } else {
            match self
                .live
                .build_signed_sell_limit_json(tok, px, count as u64, "GTC")
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[poly-maker] sell sign: {e:#}");
                    return Err(());
                }
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&js) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[poly-maker] json: {e:#}");
                return Err(());
            }
        };
        match self.live.place_batch_orders_ids(&[v]) {
            Ok(ids) if !ids.is_empty() && !ids[0].is_empty() => Ok(ids[0].clone()),
            Ok(_) => {
                eprintln!("[poly-maker] place: empty order id");
                Err(())
            }
            Err(e) => {
                eprintln!("[poly-maker] place: {e:#}");
                Err(())
            }
        }
    }
}

impl MakerAmendOps for PolyMakerOps {
    fn log_tag(&self) -> &'static str {
        "poly-maker"
    }
    fn cancel_one(&self, order_id: &str) -> bool {
        self.live.cancel_clob_order(order_id)
    }
    fn cancel_slot(&self, slot: &mut MakerOrderSlot) -> bool {
        let ids: Vec<String> = slot.all_order_ids().cloned().collect();
        if ids.is_empty() {
            return true;
        }
        let mut ok = true;
        for id in ids {
            if !self.live.cancel_clob_order(&id) {
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
        if new_count < slot.current_count {
            let restore_count = slot.current_count;
            if !self.cancel_slot(slot) {
                return MakerResizeOutcome::FailedNoChange;
            }
            match self.sign_place_one(slot.yes_price_cents, action, new_count) {
                Ok(oid) => {
                    slot.order_id = oid;
                    slot.current_count = new_count;
                    slot.child_orders.clear();
                    slot.last_matched_reported = 0.0;
                    MakerResizeOutcome::Ok
                }
                Err(()) => {
                    eprintln!(
                        "[poly-maker] downsize replace failed (wanted {}); restoring {} contracts",
                        new_count, restore_count
                    );
                    match self.sign_place_one(slot.yes_price_cents, action, restore_count) {
                        Ok(oid) => {
                            slot.order_id = oid;
                            slot.current_count = restore_count;
                            slot.child_orders.clear();
                            slot.last_matched_reported = 0.0;
                            MakerResizeOutcome::LiveAfterDownsizeRetry
                        }
                        Err(()) => {
                            eprintln!(
                                "[poly-maker] restore after failed downsize also failed ({} contracts)",
                                restore_count
                            );
                            slot.mark_dead();
                            MakerResizeOutcome::FailedNoChange
                        }
                    }
                }
            }
        } else {
            let delta = new_count - slot.current_count;
            if !poly_increase_delta_meets_min_size(delta, self.live.constraints.order_min_size) {
                let min_sz = self.live.constraints.order_min_size.max(1);
                eprintln!(
                    "[poly-maker] skip increase: delta below min_size (delta={delta} min={min_sz})"
                );
                return MakerResizeOutcome::FailedNoChange;
            }
            match self.sign_place_one(slot.yes_price_cents, action, delta) {
                Ok(oid) => {
                    slot.child_orders.push(ChildOrder {
                        order_id: oid,
                        placed_size: delta as f64,
                        last_matched: 0.0,
                    });
                    slot.current_count = new_count;
                    MakerResizeOutcome::Ok
                }
                Err(()) => MakerResizeOutcome::FailedNoChange,
            }
        }
    }
}

impl MakerVenueOps for PolyMakerOps {
    fn market(&self) -> &str {
        &self.market
    }
    fn snapshot_book_for_ipc(&mut self) -> MakerBookSnapshot {
        let book = self.live.ws_copy_orderbook();
        MakerBookSnapshot {
            version: IPC_VERSION,
            market: self.market.clone(),
            ts: now_ms(),
            bids: book
                .bids
                .iter()
                .map(|l| PriceLevel {
                    price: round_poly_price(l.price) * 100.0,
                    size: l.size,
                })
                .collect(),
            asks: book
                .asks
                .iter()
                .map(|l| PriceLevel {
                    price: round_poly_price(l.price) * 100.0,
                    size: l.size,
                })
                .collect(),
        }
    }
    fn ws_book_cents(&mut self) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let book = self.live.ws_copy_orderbook();
        poly_ws_book_to_cent_tuples(&book)
    }
    fn ws_book_as_yes_levels(&mut self) -> (Vec<PriceLevel>, Vec<PriceLevel>) {
        let book = self.live.ws_copy_orderbook();
        poly_ws_book_to_maker_cent_price_levels(&book)
    }
    fn ws_service(&mut self, ms: u64) {
        self.live.ws_service(ms);
    }
    fn ws_done(&self) -> bool {
        self.live.ws_done()
    }
    fn subscribe_fills(&mut self) {}
    fn batch_place_initial(
        &mut self,
        orders: &[CascadeOrder],
        _taker_book: &PolyFullBookPayload,
        _db: &Option<ArbDb>,
    ) -> anyhow::Result<Vec<String>> {
        let mut order_values: Vec<serde_json::Value> = Vec::new();
        for o in orders {
            let px = (o.limit_price_cents as f64) / 100.0;
            let px = if o.side == Side::Bid {
                floor_price_to_tick(px, self.live.constraints.tick)
            } else {
                ceil_price_to_tick(px, self.live.constraints.tick)
            };
            let js = if o.side == Side::Bid {
                self.live
                    .build_signed_buy_limit_json(&self.token, px, o.qty as u64, "GTC")?
            } else {
                self.live
                    .build_signed_sell_limit_json(&self.token, px, o.qty as u64, "GTC")?
            };
            order_values.push(serde_json::from_str(&js)?);
        }
        self.live.place_batch_orders_ids(&order_values)
    }
    fn touch_cascade_pivot(
        &mut self,
        pivot: &TouchCascadePivot,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        _db: &Option<ArbDb>,
    ) -> anyhow::Result<()> {
        poly_handle_touch_cascade_pivot(&self.live, &self.token, pivot, bid_slots, ask_slots)
    }
    fn poll_fill_events(
        &mut self,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        db: &Option<ArbDb>,
    ) -> Vec<MakerFillPayload> {
        let mut out = Vec::new();
        let book_touch = self.live.ws_copy_orderbook();
        let (mk_b, mk_a) = poly_ws_book_to_cent_tuples(&book_touch);
        let kb_k = k_cent_tuples_to_price_levels(&mk_b);
        let ka_k = k_cent_tuples_to_price_levels(&mk_a);
        for is_bid in [true, false] {
            let len = if is_bid {
                bid_slots.len()
            } else {
                ask_slots.len()
            };
            for i in 0..len {
                let slot = if is_bid {
                    &mut bid_slots[i]
                } else {
                    &mut ask_slots[i]
                };
                if slot.order_id.is_empty() || slot.current_count <= 0 {
                    continue;
                }
                for c in &mut slot.child_orders {
                    if let Ok(m) = self.live.data_order_matched_size(&c.order_id) {
                        c.last_matched = m;
                    }
                }
                let Some(mp) = poly_primary_matched_size_or_skip(
                    self.live.data_order_matched_size(&slot.order_id),
                    &slot.order_id,
                ) else {
                    continue;
                };
                let sum_matched = mp
                    + slot
                        .child_orders
                        .iter()
                        .map(|c| c.last_matched)
                        .sum::<f64>();
                let delta = sum_matched - slot.last_matched_reported;
                if delta <= 1e-9 {
                    slot.child_orders
                        .retain(|c| c.last_matched + 1e-9 < c.placed_size);
                    poly_sync_slot_last_matched_reported(slot, mp);
                    continue;
                }
                let cnt = delta.round().max(1.0) as u32;
                let cnt_i = cnt as i32;
                let oid = slot.order_id.clone();
                let price_cents = slot.yes_price_cents;
                slot.current_count = (slot.current_count - cnt_i).max(0);
                slot.child_orders
                    .retain(|c| c.last_matched + 1e-9 < c.placed_size);
                if slot.current_count == 0 {
                    slot.mark_dead();
                } else {
                    poly_sync_slot_last_matched_reported(slot, mp);
                }
                if let Some(db) = db {
                    let cascade = maker_cascade_overlay(bid_slots, ask_slots);
                    log_db(
                        "poly-maker.record_fill",
                        db.record_fill(
                            &self.market,
                            &kb_k,
                            &ka_k,
                            &book_touch.bids,
                            &book_touch.asks,
                            price_cents as i32,
                            cnt,
                            Some(&cascade),
                        ),
                    );
                }
                out.push(MakerFillPayload {
                    ts: now_ms(),
                    order_id: oid,
                    side: if is_bid { Side::Bid } else { Side::Ask },
                    price_cents,
                    filled_count: cnt,
                    market: self.market.clone(),
                });
            }
        }
        out
    }
    fn recalc_balance_kalshi_leg(&mut self) -> f64 {
        2000.0
    }
    fn recalc_balance_poly_leg(&mut self) -> f64 {
        self.live.get_balance()
    }
    fn db_log_tag(&self) -> &'static str {
        "poly-maker"
    }
    fn cancel_every_slot_best_effort(
        &mut self,
        bid_slots: &mut [MakerOrderSlot],
        ask_slots: &mut [MakerOrderSlot],
    ) {
        poly_cancel_all_cascade_slots(&self.live, bid_slots, ask_slots);
    }
    fn books_for_abort_log(
        &mut self,
    ) -> (
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
    ) {
        let book_end = self.live.ws_copy_orderbook();
        let (mk_b_end, mk_a_end) = poly_ws_book_to_cent_tuples(&book_end);
        let kb_k = k_cent_tuples_to_price_levels(&mk_b_end);
        let ka_k = k_cent_tuples_to_price_levels(&mk_a_end);
        (kb_k, ka_k, book_end.bids.clone(), book_end.asks.clone())
    }
}

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
    pub initial_taker_vol: f64,
    pub original_order_qty: u32,
}

#[derive(Debug, Clone)]
struct PreSignedOrder {
    value: serde_json::Value,
    size: u64,
}

#[derive(Debug)]
struct PoolSlot {
    size: u64,
    orders: Vec<PreSignedOrder>,
}

pub(crate) struct PreSignedPool {
    sell_slots: Vec<PoolSlot>,
    buy_slots: Vec<PoolSlot>,
    /// Powers-of-two ≥ `order_min_size`, used for greedy hedge decomposition.
    size_schedule: Vec<u64>,
}

impl PreSignedPool {
    fn allowed_sizes(&self) -> Vec<u64> {
        self.size_schedule.clone()
    }

    fn build(live: &PolyLive, total_volume: f64) -> anyhow::Result<Self> {
        let min_sz = live.constraints.order_min_size.max(1);
        let unit = (total_volume / PRESIGN_COPIES as f64)
            .floor()
            .max(min_sz as f64)
            .max(1.0) as u64;
        let max_bit = if unit == 0 {
            0
        } else {
            63 - (unit.leading_zeros())
        };

        let mut size_schedule: Vec<u64> = Vec::new();
        for bit in 0..=max_bit {
            let s = 1u64 << bit;
            if s >= min_sz {
                size_schedule.push(s);
            }
        }
        if size_schedule.is_empty() {
            size_schedule.push(min_sz);
        }
        size_schedule.sort_unstable();

        let token = &live.token_id;
        let mut sell_slots = Vec::new();
        let mut buy_slots = Vec::new();

        for &size in &size_schedule {
            let mut sells = Vec::new();
            let mut buys = Vec::new();
            for _ in 0..PRESIGN_COPIES {
                let sell_val = live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?;
                sells.push(PreSignedOrder {
                    value: sell_val,
                    size,
                });

                let buy_val = live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?;
                buys.push(PreSignedOrder {
                    value: buy_val,
                    size,
                });
            }
            sell_slots.push(PoolSlot {
                size,
                orders: sells,
            });
            buy_slots.push(PoolSlot { size, orders: buys });
        }

        eprintln!(
            "[poly] pre-signed pool: min_order={min_sz} unit={unit} max_bit={max_bit} slots={} orders_per_side={}",
            size_schedule.len(),
            size_schedule.len() * PRESIGN_COPIES
        );

        Ok(PreSignedPool {
            sell_slots,
            buy_slots,
            size_schedule,
        })
    }

    fn decompose_and_take(&mut self, count: u64, is_sell: bool) -> Vec<PreSignedOrder> {
        let slots = if is_sell {
            &mut self.sell_slots
        } else {
            &mut self.buy_slots
        };
        let plan = decompose_order_sizes(count, self.size_schedule.clone());
        let mut orders = Vec::new();
        for need_size in plan {
            if let Some(slot) = slots.iter_mut().find(|s| s.size == need_size) {
                if let Some(o) = slot.orders.pop() {
                    orders.push(o);
                    continue;
                }
            }
            eprintln!(
                "[poly] pool exhausted, no presign for size={}, plan incomplete",
                need_size
            );
            break;
        }

        let got: u64 = orders.iter().map(|o| o.size).sum();
        if got < count {
            eprintln!(
                "[poly] pool exhausted, {} remaining unfilled from pre-signed",
                count.saturating_sub(got)
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
        let slots = if hedge_was_sell_on_poly {
            &mut self.sell_slots
        } else {
            &mut self.buy_slots
        };
        let token = &live.token_id;
        for slot in slots.iter_mut() {
            if !slot.orders.is_empty() {
                continue;
            }
            let size = slot.size;
            for _ in 0..PRESIGN_COPIES {
                let val = if hedge_was_sell_on_poly {
                    live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?
                } else {
                    live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?
                };
                slot.orders.push(PreSignedOrder { value: val, size });
            }
            eprintln!("[poly] refilled exhausted presign slot size={size}");
        }
        Ok(())
    }

    /// Batch top-up any slot with count < PRESIGN_COPIES on both sides (debounced).
    fn batch_refill_low_slots(&mut self, live: &PolyLive) -> anyhow::Result<()> {
        let token = &live.token_id;
        for slot in &mut self.sell_slots {
            let size = slot.size;
            while slot.orders.len() < PRESIGN_COPIES {
                let val = live.build_signed_sell_order_value(token, HEDGE_SELL_PRICE, size)?;
                slot.orders.push(PreSignedOrder { value: val, size });
            }
        }
        for slot in &mut self.buy_slots {
            let size = slot.size;
            while slot.orders.len() < PRESIGN_COPIES {
                let val = live.build_signed_buy_order_value(token, HEDGE_BUY_PRICE, size)?;
                slot.orders.push(PreSignedOrder { value: val, size });
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
    /// Copy of cascade sizing inputs (for touch pivots).
    pub kalshi_balance: f64,
    pub poly_balance: f64,
    pub side_cap: f64,
}

pub(crate) fn volume_above(st: &PolyState, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    st.bids
        .iter()
        .filter(|l| l.price > thr)
        .map(|l| l.size)
        .sum()
}
pub(crate) fn volume_below(st: &PolyState, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    st.asks
        .iter()
        .filter(|l| l.price < thr)
        .map(|l| l.size)
        .sum()
}

pub fn setup_tracked(ctx: &mut PolyContext, levels: &MakerLevelsDonePayload) {
    ctx.tracked_bid.clear();
    ctx.tracked_ask.clear();
    for (pc, placed) in levels.bid_levels.iter().take(ARB_MAX_TRACKED) {
        let v = volume_above(&ctx.state, *pc);
        let oq = placed.floor().max(1.0) as u32;
        ctx.tracked_bid.push(TrackedLevel {
            side: Side::Bid,
            price_cents: *pc,
            last_sent_vol: v,
            initial_taker_vol: v,
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
            initial_taker_vol: v,
            original_order_qty: oq,
        });
    }
}

pub(crate) fn rebaseline_tracked(ctx: &mut PolyContext, side: Side) {
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

fn merge_kalshi_touch_into_ladder(ctx: &mut PolyContext, touch: &MakerTouchChanged) {
    match touch.side {
        Side::Bid => {
            if let Some(l) = ctx
                .kalshi_bids
                .iter_mut()
                .find(|l| (l.price + 0.5) as i16 == touch.new_k_cents)
            {
                l.size = touch.new_k_qty;
            } else {
                ctx.kalshi_bids.push(PriceLevel {
                    price: touch.new_k_cents as f64,
                    size: touch.new_k_qty,
                });
            }
            ctx.kalshi_bids.sort_by(|a, b| {
                b.price
                    .partial_cmp(&a.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        Side::Ask => {
            if let Some(l) = ctx
                .kalshi_asks
                .iter_mut()
                .find(|l| (l.price + 0.5) as i16 == touch.new_k_cents)
            {
                l.size = touch.new_k_qty;
            } else {
                ctx.kalshi_asks.push(PriceLevel {
                    price: touch.new_k_cents as f64,
                    size: touch.new_k_qty,
                });
            }
            ctx.kalshi_asks.sort_by(|a, b| {
                a.price
                    .partial_cmp(&b.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

/// Kalshi market touch moved: if arb rules pass vs Poly book, drop lowest tracked tier and open a new one at the touch.
pub(crate) fn handle_kalshi_touch_changed(
    ctx: &mut PolyContext,
    touch: &MakerTouchChanged,
    market: &str,
    fd_out: RawFd,
) -> anyhow::Result<()> {
    merge_kalshi_touch_into_ladder(ctx, touch);

    let poly_book = PolyFullBookPayload {
        bids: ctx.state.bids.clone(),
        asks: ctx.state.asks.clone(),
    };

    let st = MakerState {
        yes_bids: ctx.kalshi_bids.clone(),
        yes_asks: ctx.kalshi_asks.clone(),
        kalshi_balance: ctx.kalshi_balance,
        poly_balance: ctx.poly_balance,
        side_cap: ctx.side_cap,
    };

    let tracked = if touch.side == Side::Bid {
        &ctx.tracked_bid
    } else {
        &ctx.tracked_ask
    };
    if tracked.is_empty() {
        return Ok(());
    }
    let new_tracked_cents = match touch.side {
        Side::Bid => touch.new_k_cents,
        Side::Ask => touch.new_k_cents.saturating_sub(1),
    };
    if tracked.iter().any(|t| t.price_cents == new_tracked_cents) {
        return Ok(());
    }

    // Check if new level qualifies before doing anything with dropping.
    let sized = match touch.side {
        Side::Bid => {
            strategy::cascade_bid_touch_size(&st, &poly_book, touch.new_k_cents, touch.new_k_qty)
        }
        Side::Ask => {
            strategy::cascade_ask_touch_size(&st, &poly_book, touch.new_k_cents, touch.new_k_qty)
        }
    };
    let Some((req, poly_vol)) = sized else {
        return Ok(());
    };
    if req < 1.0 {
        return Ok(());
    }

    // Keep the side-specific cascade window near touch:
    // - bids: drop lowest tracked level
    // - asks: drop highest tracked level
    let drop_cents = if touch.side == Side::Bid {
        tracked
            .iter()
            .map(|t| t.price_cents)
            .min()
            .expect("non-empty tracked")
    } else {
        tracked
            .iter()
            .map(|t| t.price_cents)
            .max()
            .expect("non-empty tracked")
    };

    let qty = req.floor().max(1.0) as u32;
    let (level_price_cents, limit_price_cents) = if touch.side == Side::Bid {
        let lim = (touch.new_k_cents + 1).min(99);
        (touch.new_k_cents, lim)
    } else {
        let lv = touch.new_k_cents.saturating_sub(1);
        (lv, lv)
    };
    let new_order = CascadeOrder {
        level_price_cents,
        side: touch.side,
        limit_price_cents,
        qty,
        initial_taker_vol: poly_vol,
        initial_maker_vol: touch.new_k_qty,
    };

    let pivot = TouchCascadePivot {
        version: IPC_VERSION,
        ts: now_ms(),
        market: market.to_string(),
        side: touch.side,
        drop_level_price_cents: drop_cents,
        new_order,
    };

    let tr = if touch.side == Side::Bid {
        &mut ctx.tracked_bid
    } else {
        &mut ctx.tracked_ask
    };
    tr.retain(|t| t.price_cents != drop_cents);
    let v_init = if touch.side == Side::Bid {
        volume_above(&ctx.state, touch.new_k_cents)
    } else {
        volume_below(&ctx.state, new_tracked_cents)
    };
    tr.push(TrackedLevel {
        side: touch.side,
        price_cents: new_tracked_cents,
        last_sent_vol: v_init,
        initial_taker_vol: v_init,
        original_order_qty: qty,
    });

    eprintln!(
        "[poly] Kalshi touch pivot {:?}: drop {}¢ → new {}¢ x{} (poly vol {:.2})",
        touch.side, drop_cents, new_tracked_cents, qty, poly_vol
    );
    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TouchCascadePivot(pivot)) {
        eprintln!("[poly] TouchCascadePivot IPC send failed: {e:#}");
        return Err(e).context("TouchCascadePivot send");
    }
    Ok(())
}

/// True when volume change exceeds 15% increase or decrease (for Poly→Kalshi vol updates).
fn poly_vol_threshold_15pct(last: f64, current: f64) -> bool {
    if last > 0.0 {
        let delta = current - last;
        let frac = delta / last;
        if frac < 0.0 && (-frac) >= 0.15 {
            return true;
        }
        if frac >= 0.15 {
            return true;
        }
    } else if current > 0.0 {
        return true;
    }
    false
}

/// When any tracked level on `side` crosses 15% vol band, emit `PolyLevelVolUpdate` with raw volumes.
/// Kalshi performs all volume/price readjustments; Poly only sends data when threshold is hit.
pub(crate) fn compute_poly_vol_update(
    ctx: &mut PolyContext,
    side: Side,
) -> Option<TakerLevelVolUpdatePayload> {
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
        if poly_vol_threshold_15pct(t.last_sent_vol, current) {
            any = true;
            break;
        }
    }
    if !any {
        return None;
    }

    let mut levels = Vec::new();
    for (t, current) in tracked.iter_mut().zip(currents) {
        t.last_sent_vol = current;
        levels.push((t.price_cents, current));
    }

    let poly_book = PolyFullBookPayload {
        bids: ctx.state.bids.clone(),
        asks: ctx.state.asks.clone(),
    };
    Some(TakerLevelVolUpdatePayload {
        side,
        levels,
        taker_book: Some(poly_book),
    })
}

/// When Polymarket best bid or best ask moves, immediately ship the full Poly book to Kalshi so it
/// can re-run `build_cascade` (same IPC shape as the 15% depth threshold path).
pub(crate) fn poly_touch_book_update(ctx: &mut PolyContext) -> Option<TakerLevelVolUpdatePayload> {
    if ctx.tracked_bid.is_empty() && ctx.tracked_ask.is_empty() {
        return None;
    }
    for t in ctx.tracked_bid.iter_mut() {
        t.last_sent_vol = volume_above(&ctx.state, t.price_cents);
    }
    for t in ctx.tracked_ask.iter_mut() {
        t.last_sent_vol = volume_below(&ctx.state, t.price_cents);
    }
    let poly_book = PolyFullBookPayload {
        bids: ctx.state.bids.clone(),
        asks: ctx.state.asks.clone(),
    };
    Some(TakerLevelVolUpdatePayload {
        side: Side::Bid,
        levels: vec![],
        taker_book: Some(poly_book),
    })
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
        "poly.AbortFatal",
    );
}

pub(crate) fn place_hedge_orders(
    live: &PolyLive,
    fill: &MakerFillPayload,
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
        for slot_size in decompose_order_sizes(shortfall, pool.allowed_sizes()) {
            let val = if is_sell {
                live.build_signed_sell_order_value(token, price, slot_size)
            } else {
                live.build_signed_buy_order_value(token, price, slot_size)
            };
            match val {
                Ok(v) => orders.push(PreSignedOrder {
                    value: v,
                    size: slot_size,
                }),
                Err(e) => {
                    return Some(format!("hedge_sign_fallback_failed:{e}"));
                }
            }
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

    if let Err(e) = pool.refill_exhausted_slots(live, is_sell) {
        eprintln!("[poly] hedge refill_exhausted_slots: {e:#}");
    }

    if total_size < count {
        return Some(format!(
            "hedge_incomplete:placed={total_size},needed={count}"
        ));
    }
    if total_size > count {
        eprintln!(
            "[poly] hedge batch placed extra contracts (min slot / padding): placed={total_size} needed={count}"
        );
    }
    None
}

/// Polymarket as resting maker. Kalshi child runs the taker loop — same control flow as `kalshi_maker_process_run`.
fn poly_maker_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if let Err(e) = shutdown::install_shutdown_handler() {
        eprintln!("[poly-maker] shutdown handler: {e:#}");
    }
    let creds = ArbCreds::from_env();
    let token = env::var("ARB_TOKEN_ID").context("ARB_TOKEN_ID not set")?;
    let ticker = env::var("ARB_TICKER").unwrap_or_default();
    let market = ticker.clone();
    let config_neg = matches!(
        env::var("ARB_NEG_RISK").unwrap_or_default().as_str(),
        "1" | "true" | "yes"
    );
    let constraints = PolyClobConstraints::fetch_for_clob_token(&token)
        .unwrap_or_else(|_| PolyClobConstraints::legacy_from_config(config_neg));
    eprintln!(
        "[poly-maker] connecting WS for token={}",
        &token[..16.min(token.len())]
    );
    let mut live = PolyLive::new(&creds, &token, constraints);
    eprintln!(
        "[poly-maker] CLOB EIP-712 signatureType={} (set POLY_SIGNATURE_TYPE: 0=EOA, 1=POLY_PROXY if order POST returns 400)",
        live.signature_type
    );
    live.ws_connect()?;
    let mut ops = PolyMakerOps {
        live,
        market,
        token,
    };
    maker_run(&mut ops, fd_in, fd_out, &creds)
}

pub fn poly_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if crate::arb_config::env_maker_is_polymarket() {
        return poly_maker_process_run(fd_in, fd_out);
    }
    poly_taker_process_run(fd_in, fd_out)
}

pub fn poly_taker_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    if let Err(e) = shutdown::install_shutdown_handler() {
        eprintln!("[poly] shutdown handler install failed: {e:#}");
    }

    let creds = ArbCreds::from_env();
    let token = env::var("ARB_TOKEN_ID").context("ARB_TOKEN_ID not set")?;
    let ticker = env::var("ARB_TICKER").unwrap_or_default();
    let market = ticker.clone();
    let config_neg = matches!(
        env::var("ARB_NEG_RISK").unwrap_or_default().as_str(),
        "1" | "true" | "yes"
    );

    let constraints = match PolyClobConstraints::fetch_for_clob_token(&token) {
        Ok(c) => {
            if c.neg_risk != config_neg {
                eprintln!(
                    "[poly] Gamma neg_risk={} (config had {}; using Gamma for signing)",
                    c.neg_risk, config_neg
                );
            }
            eprintln!(
                "[poly] Gamma CLOB: tick={} order_min_size={} neg_risk={}",
                format_poly_price_for_tick(c.tick, c.tick),
                c.order_min_size,
                c.neg_risk
            );
            c
        }
        Err(e) => {
            eprintln!(
                "[poly] Gamma constraints unavailable ({e:#}); using config neg_risk={} tick=0.01 min_size=1",
                config_neg
            );
            PolyClobConstraints::legacy_from_config(config_neg)
        }
    };

    eprintln!(
        "[poly] connecting WS for token={}",
        &token[..16.min(token.len())]
    );
    let mut live = PolyLive::new(&creds, &token, constraints);
    eprintln!(
        "[poly] CLOB EIP-712 signatureType={} (set POLY_SIGNATURE_TYPE: 0=EOA, 1=POLY_PROXY if order POST returns 400)",
        live.signature_type
    );
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

    eprintln!(
        "[poly] book: {} bids, {} asks | best_bid={} best_ask={}",
        ctx.state.bids.len(),
        ctx.state.asks.len(),
        format_poly_price_for_tick(ctx.state.best_bid, live.constraints.tick),
        format_poly_price_for_tick(ctx.state.best_ask, live.constraints.tick),
    );

    let msg: ArbMsg = match arb_ipc::recv_msg(fd_in) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[poly] recv MakerBookSnapshot: {e:#}");
            return Err(e).context("poly recv kalshi snapshot");
        }
    };
    let snap = match msg {
        ArbMsg::MakerBookSnapshot(s) => s,
        ArbMsg::MakerAbort { reason } => {
            eprintln!("[poly] kalshi abort before snapshot: {reason}");
            return Ok(());
        }
        other => anyhow::bail!("[poly] expected MakerBookSnapshot, got {other:?}"),
    };

    ctx.kalshi_bids = snap.bids.clone();
    ctx.kalshi_asks = snap.asks.clone();

    let mut st = MakerState::init();
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
        eprintln!("[poly] no cascade levels — sending empty CascadeOrders so Kalshi can unblock");
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
        eprintln!("[poly] send CascadeOrders: {e:#}");
        return Err(e).context("send cascade");
    }

    if levels.bid_levels.is_empty() && levels.ask_levels.is_empty() {
        return Ok(());
    }

    let msg2: ArbMsg = match arb_ipc::recv_msg(fd_in) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[poly] recv KalshiLevelsDone: {e:#}");
            return Err(e).context("poly recv levels");
        }
    };
    let levels_done = match msg2 {
        ArbMsg::MakerLevelsDone(l) => l,
        ArbMsg::MakerAbort { reason } => {
            eprintln!("[poly] kalshi aborted: {reason}");
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
                Err(e) => eprintln!("[poly] ArbDb::open (kalshi abort path): {e:#}"),
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

    let no_token_merge = env::var("ARB_NO_TOKEN_ID").unwrap_or_default();
    let polygon_rpc = crate::poly_merge::polygon_rpc_url();
    let merge_condition_id = match crate::poly_merge::gamma_condition_id_for_clob_token(&token) {
        Ok(cid) => Some(cid),
        Err(e) => {
            eprintln!("[poly] idle merge disabled: Gamma conditionId lookup failed ({e:#})");
            None
        }
    };
    let merge_idle_enabled = merge_condition_id.is_some() && !no_token_merge.trim().is_empty();
    if merge_condition_id.is_some() && no_token_merge.trim().is_empty() {
        eprintln!(
            "[poly] idle merge disabled: set ARB_NO_TOKEN_ID (config polymarket_no_token_id) for the NO leg"
        );
    }

    let mut last_trade_ms: u64 = 0;
    let mut pending_signed: i32 = 0;
    const POLY_BEST_TOUCH_EPS: f64 = 1e-9;
    let mut last_poly_bb: Option<f64> = None;
    let mut last_poly_ba: Option<f64> = None;

    loop {
        if shutdown::shutdown_requested() {
            eprintln!("[poly] shutdown requested");
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
            let reason = "poly_ws_disconnected".to_string();
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
                eprintln!("[poly] ws_disconnect Abort IPC send failed: {e:#}");
                return Err(e).context("poly ws_disconnect Abort");
            }
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

        if last_trade_ms > 0 && now_ms().saturating_sub(last_trade_ms) >= RESIGN_DEBOUNCE_MS {
            if let Some(ref mut p) = pool {
                if let Err(e) = p.batch_refill_low_slots(&live) {
                    eprintln!("[poly] batch_refill_low_slots: {e:#}");
                }
            }

            if merge_idle_enabled {
                if let Some(ref cid) = merge_condition_id {
                    let yes_sz = live.get_position(&token);
                    let no_sz = live.get_position(&no_token_merge);
                    let relayer_cfg = creds.relayer_config();
                    match crate::poly_merge::try_merge_idle_yes_no(
                        &polygon_rpc,
                        &creds.eth_priv_key,
                        &creds.poly_address,
                        cid,
                        yes_sz,
                        no_sz,
                        creds.poly_signature_type,
                        relayer_cfg.as_ref(),
                    ) {
                        Ok(Some(_)) | Ok(None) => {
                            last_trade_ms = 0;
                        }
                        Err(e) => {
                            eprintln!("[poly] idle CTF merge error: {e:#}");
                            last_trade_ms = now_ms();
                        }
                    }
                }
            } else {
                last_trade_ms = 0;
            }
        }

        if ctx.state.best_bid > 0.95 || ctx.state.best_ask < 0.05 {
            let reason = format!(
                "price_out_of_band bid={} ask={}",
                format_poly_price_for_tick(ctx.state.best_bid, live.constraints.tick),
                format_poly_price_for_tick(ctx.state.best_ask, live.constraints.tick),
            );
            eprintln!("[poly] {reason}");
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
                        "[poly] best touch moved bid {}→{} ask {}→{}: PolyLevelVolUpdate (full book) → Kalshi",
                        format_poly_price_for_tick(lb, live.constraints.tick),
                        format_poly_price_for_tick(bb, live.constraints.tick),
                        format_poly_price_for_tick(la, live.constraints.tick),
                        format_poly_price_for_tick(ba, live.constraints.tick),
                    );
                    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                        eprintln!("[poly] PolyLevelVolUpdate (touch) IPC send failed: {e:#}");
                        return Err(e).context("poly PolyLevelVolUpdate touch");
                    }
                }
            }
            last_poly_bb = Some(bb);
            last_poly_ba = Some(ba);
        }

        if let Some(u) = compute_poly_vol_update(&mut ctx, Side::Bid) {
            if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                eprintln!("[poly] PolyLevelVolUpdate bid IPC send failed: {e:#}");
                return Err(e).context("poly PolyLevelVolUpdate bid");
            }
        }
        if let Some(u) = compute_poly_vol_update(&mut ctx, Side::Ask) {
            if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::TakerLevelVolUpdate(u)) {
                eprintln!("[poly] PolyLevelVolUpdate ask IPC send failed: {e:#}");
                return Err(e).context("poly PolyLevelVolUpdate ask");
            }
        }

        let ipc_ready = match arb_ipc::poll_readable(fd_in, 0) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[poly] poll_readable: {e:#}");
                false
            }
        };
        if ipc_ready {
            let m: ArbMsg = match arb_ipc::recv_msg(fd_in) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[poly] main loop IPC recv: {e:#}");
                    return Err(e).context("poly main loop recv");
                }
            };
            match m {
                ArbMsg::MakerAbort { reason } => {
                    eprintln!("[poly] kalshi abort: {reason}");
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
                    eprintln!("[poly] abort_fatal from kalshi: {}", a.message);
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
                        "[poly] fill: {:?} x{} at {}c oid={}",
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
                        let synth = MakerFillPayload {
                            ts: now_ms(),
                            order_id: "accum".into(),
                            side: if q > 0 { Side::Ask } else { Side::Bid },
                            price_cents: 0,
                            filled_count: q.unsigned_abs(),
                            market: market.clone(),
                        };
                        if let Some(ref mut p) = pool {
                            place_hedge_orders(&live, &synth, p)
                        } else {
                            let mut fallback =
                                match PreSignedPool::build(&live, synth.filled_count as f64) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        let reason = format!("hedge_pool_build_failed:{e}");
                                        eprintln!("[poly] {reason}");
                                        if let Some(db) = &db {
                                            log_db(
                                                "poly.record_abort.hedge_pool_build",
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
                                        send_abort_fatal(fd_out, "pool_build", &reason);
                                        break;
                                    }
                                };
                            place_hedge_orders(&live, &synth, &mut fallback)
                        }
                    } else {
                        None
                    };
                    if let Some(abort_reason) = hedge_err {
                        eprintln!("[poly] hedge failed: {abort_reason}");
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
                        "[poly] hedge placed for {} x{}, rebaselined {:?} tracked levels",
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
            initial_taker_vol: 1.0,
            original_order_qty: 1,
        }];
        ctx.state.bids.insert(
            0,
            PriceLevel {
                price: 0.995,
                size: 2.0,
            },
        );
        let u = compute_poly_vol_update(&mut ctx, Side::Bid).unwrap();
        assert_eq!(u.levels.len(), 1);
        assert_eq!(u.levels[0].0, 99);
        assert!(u.levels[0].1 > 0.0);
    }

    #[test]
    fn sends_update_on_drop_over_15pct() {
        let mut ctx = ctx_with_book();
        ctx.tracked_bid = vec![TrackedLevel {
            side: Side::Bid,
            price_cents: 50,
            last_sent_vol: 20.0,
            initial_taker_vol: 20.0,
            original_order_qty: 10,
        }];
        ctx.state.bids.retain(|l| l.price != 0.59);
        // Poly volume above 50¢: only 0.60 bid remains → 10 (was 20). 50% drop exceeds 15% threshold.
        // PolyLevelVolUpdate carries raw poly volume; Kalshi does the qty calculation.
        let u = compute_poly_vol_update(&mut ctx, Side::Bid).unwrap();
        assert_eq!(u.levels.len(), 1);
        assert_eq!(u.levels[0].0, 50);
        assert!((u.levels[0].1 - 10.0).abs() < 1e-9);
    }
}

#[cfg(test)]
#[test]
fn poly_last_matched_baseline_follows_remaining_orders_after_child_gc() {
    use crate::maker_runtime::{ChildOrder, MakerOrderSlot};
    let mut slot = MakerOrderSlot {
        order_id: "primary".into(),
        level_price_cents: 50,
        yes_price_cents: 51,
        original_count: 20,
        current_count: 10,
        initial_taker_vol: 0.0,
        initial_maker_vol: 0.0,
        child_orders: vec![ChildOrder {
            order_id: "c1".into(),
            placed_size: 5.0,
            last_matched: 5.0,
        }],
        // Stale high watermark after summing primary+child before GC:
        last_matched_reported: 15.0,
    };
    let mp = 10.0_f64;
    slot.child_orders
        .retain(|c| c.last_matched + 1e-9 < c.placed_size);
    poly_sync_slot_last_matched_reported(&mut slot, mp);
    assert!((slot.last_matched_reported - 10.0).abs() < 1e-9);
}

#[cfg(test)]
#[test]
fn poly_primary_matched_read_error_skips_baseline_update() {
    let slot = MakerOrderSlot {
        order_id: "primary".into(),
        level_price_cents: 50,
        yes_price_cents: 51,
        original_count: 20,
        current_count: 10,
        initial_taker_vol: 0.0,
        initial_maker_vol: 0.0,
        child_orders: vec![],
        last_matched_reported: 15.0,
    };
    let old_baseline = slot.last_matched_reported;

    assert!(poly_primary_matched_size_or_skip(
        Err(anyhow::anyhow!("temporary read failure")),
        &slot.order_id
    )
    .is_none());
    assert_eq!(slot.last_matched_reported, old_baseline);
}
