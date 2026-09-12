//! Shared maker-loop helpers: cascade slots, periodic recalc, and volume / level IPC paths.

use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::{log_db, ArbDb, KalshiCascadeOverlay};
use crate::arb_ipc;
use crate::shutdown;
use crate::strategy::{build_cascade, MakerState};
use crate::types::*;
use anyhow::Context;
use std::collections::HashMap;
use std::os::unix::io::RawFd;

/// One additional resting child order (Polymarket delta-amend only).
#[derive(Debug, Clone)]
pub struct ChildOrder {
    pub order_id: String,
    pub placed_size: f64,
    pub last_matched: f64,
}

/// One resting maker order tracked across amend/Cancel IPC and fills.
#[derive(Debug, Clone)]
pub struct MakerOrderSlot {
    pub order_id: String,
    /// Level key matching the taker-side tracked price (k_bid / k_ask).
    pub level_price_cents: i16,
    /// Limit price in yes-cents on the maker venue.
    pub yes_price_cents: i16,
    pub original_count: i32,
    pub current_count: i32,
    pub initial_taker_vol: f64,
    pub initial_maker_vol: f64,
    /// Polymarket: extra delta orders at same price (native amend N/A).
    pub child_orders: Vec<ChildOrder>,
    /// Polymarket: last summed `data_order_matched_size` across primary + children.
    pub last_matched_reported: f64,
}

impl Default for MakerOrderSlot {
    fn default() -> Self {
        Self {
            order_id: String::new(),
            level_price_cents: 0,
            yes_price_cents: 0,
            original_count: 0,
            current_count: 0,
            initial_taker_vol: 0.0,
            initial_maker_vol: 0.0,
            child_orders: Vec::new(),
            last_matched_reported: 0.0,
        }
    }
}

impl MakerOrderSlot {
    /// Primary plus child order ids (non-empty only for primary).
    pub fn all_order_ids(&self) -> impl Iterator<Item = &String> + '_ {
        std::iter::once(&self.order_id)
            .filter(|s| !s.is_empty())
            .chain(self.child_orders.iter().map(|c| &c.order_id))
    }

    pub fn has_primary_order(&self) -> bool {
        !self.order_id.is_empty()
    }

    pub fn mark_dead(&mut self) {
        self.order_id.clear();
        self.child_orders.clear();
        self.current_count = 0;
        self.last_matched_reported = 0.0;
    }
}

/// Result of trying to move a maker slot to `new_count` resting contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakerResizeOutcome {
    /// Resting size now matches `new_count` (or was already).
    Ok,
    /// Target not reached; venue-side resting state is unchanged from before this call.
    FailedNoChange,
    /// Polymarket downsize: replacement open failed but prior size was re-placed; reanchor vol baselines only in this case.
    LiveAfterDownsizeRetry,
}

impl MakerResizeOutcome {
    #[inline]
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

pub trait MakerAmendOps {
    fn log_tag(&self) -> &'static str;
    fn cancel_one(&self, order_id: &str) -> bool;
    /// Cancel every resting id for this slot; on full success clears primary + children locally.
    fn cancel_slot(&self, slot: &mut MakerOrderSlot) -> bool;
    fn resize_slot(
        &self,
        slot: &mut MakerOrderSlot,
        action: &str,
        new_count: i32,
    ) -> MakerResizeOutcome;
}

pub fn maker_cascade_overlay(
    bid_slots: &[MakerOrderSlot],
    ask_slots: &[MakerOrderSlot],
) -> KalshiCascadeOverlay {
    let mut bids = HashMap::new();
    for s in bid_slots {
        if s.current_count > 0 && s.has_primary_order() {
            *bids.entry(s.yes_price_cents).or_insert(0.0) += s.current_count as f64;
        }
    }
    let mut asks = HashMap::new();
    for s in ask_slots {
        if s.current_count > 0 && s.has_primary_order() {
            *asks.entry(s.yes_price_cents).or_insert(0.0) += s.current_count as f64;
        }
    }
    KalshiCascadeOverlay::from_limit_maps(bids, asks)
}

/// Convert taker book (US dollars / share) to `(price_cents, size)` tuples like Kalshi `ws_copy_orderbook`.
pub fn taker_book_to_k_cent_tuples(tb: &PolyFullBookPayload) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let k_bids: Vec<(f64, f64)> = tb.bids.iter().map(|l| (l.price * 100.0, l.size)).collect();
    let k_asks: Vec<(f64, f64)> = tb.asks.iter().map(|l| (l.price * 100.0, l.size)).collect();
    (k_bids, k_asks)
}

pub fn k_cent_tuples_to_price_levels(k: &[(f64, f64)]) -> Vec<PriceLevel> {
    k.iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect()
}

pub fn orders_to_maker_slots(
    orders: &[CascadeOrder],
    ids: &[String],
) -> anyhow::Result<(Vec<MakerOrderSlot>, Vec<MakerOrderSlot>)> {
    if ids.len() != orders.len() {
        anyhow::bail!(
            "maker order id mismatch: got {} ids for {} orders",
            ids.len(),
            orders.len()
        );
    }
    let mut bid_slots = Vec::new();
    let mut ask_slots = Vec::new();
    for (i, co) in orders.iter().enumerate() {
        let oid = ids[i].clone();
        if oid.is_empty() {
            anyhow::bail!("empty maker order id at index {i}");
        }
        let slot = MakerOrderSlot {
            order_id: oid,
            level_price_cents: co.level_price_cents,
            yes_price_cents: co.limit_price_cents,
            original_count: co.qty as i32,
            current_count: co.qty as i32,
            initial_taker_vol: co.initial_taker_vol,
            initial_maker_vol: co.initial_maker_vol,
            child_orders: Vec::new(),
            last_matched_reported: 0.0,
        };
        if co.side == Side::Bid {
            bid_slots.push(slot);
        } else {
            ask_slots.push(slot);
        }
    }
    Ok((bid_slots, ask_slots))
}

pub fn maker_vol_at_price(book: &[(f64, f64)], price_cents: i16) -> f64 {
    book.iter()
        .find(|(p, _)| (p.round() as i16) == price_cents)
        .map(|(_, s)| *s)
        .unwrap_or(0.0)
}

pub fn find_level_price(slots: &[MakerOrderSlot], oid: &str) -> (i16, i16) {
    for s in slots {
        if s.order_id == oid {
            return (s.level_price_cents, s.yes_price_cents);
        }
        for c in &s.child_orders {
            if c.order_id == oid {
                return (s.level_price_cents, s.yes_price_cents);
            }
        }
    }
    (0, 0)
}

/// Best YES bid from raw WS book `(¢, size)`.
pub fn best_yes_bid_touch(bids: &[(f64, f64)]) -> Option<(i16, f64)> {
    if bids.is_empty() {
        return None;
    }
    bids.iter()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|&(p, s)| ((p + 0.5) as i16, s))
}

/// Best YES ask from raw WS book `(¢, size)`.
pub fn best_yes_ask_touch(asks: &[(f64, f64)]) -> Option<(i16, f64)> {
    if asks.is_empty() {
        return None;
    }
    asks.iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|&(p, s)| ((p + 0.5) as i16, s))
}

pub fn top_active_bid_slot(slots: &[MakerOrderSlot]) -> Option<&MakerOrderSlot> {
    slots
        .iter()
        .filter(|s| s.current_count > 0 && s.has_primary_order())
        .max_by_key(|s| s.level_price_cents)
}

pub fn top_active_ask_slot(slots: &[MakerOrderSlot]) -> Option<&MakerOrderSlot> {
    slots
        .iter()
        .filter(|s| s.current_count > 0 && s.has_primary_order())
        .min_by_key(|s| s.level_price_cents)
}

pub fn bid_touch_moved_only_by_own_limit(
    last_market_bid: Option<i16>,
    new_best_bid: i16,
    bid_slots: &[MakerOrderSlot],
) -> bool {
    let Some(slot) = top_active_bid_slot(bid_slots) else {
        return false;
    };
    last_market_bid == Some(slot.level_price_cents) && new_best_bid == slot.yes_price_cents
}

pub fn ask_touch_moved_only_by_own_limit(
    last_market_ask: Option<i16>,
    new_best_ask: i16,
    ask_slots: &[MakerOrderSlot],
) -> bool {
    let Some(slot) = top_active_ask_slot(ask_slots) else {
        return false;
    };
    last_market_ask == Some(slot.level_price_cents.saturating_add(1))
        && new_best_ask == slot.yes_price_cents
}

pub fn recalc_cascade_volumes<O: MakerAmendOps>(
    ops: &O,
    bid_slots: &mut Vec<MakerOrderSlot>,
    ask_slots: &mut Vec<MakerOrderSlot>,
    k_bids: &[(f64, f64)],
    k_asks: &[(f64, f64)],
    kalshi_balance: f64,
    poly_balance: f64,
    side_cap: f64,
) {
    let tag = ops.log_tag();
    let portfolio = kalshi_balance.min(poly_balance);
    let side_budget = (0.5 * portfolio).min(side_cap.max(1.0));

    let process_side = |slots: &mut Vec<MakerOrderSlot>, book: &[(f64, f64)], is_bid: bool| {
        let action = if is_bid { "buy" } else { "sell" };
        let mut indices: Vec<usize> = (0..slots.len()).collect();
        if is_bid {
            indices.sort_by(|&a, &b| slots[b].level_price_cents.cmp(&slots[a].level_price_cents));
        } else {
            indices.sort_by(|&a, &b| slots[a].level_price_cents.cmp(&slots[b].level_price_cents));
        }
        let active: Vec<usize> = indices
            .into_iter()
            .filter(|&i| slots[i].has_primary_order())
            .collect();

        if active.is_empty() {
            return;
        }

        let n = active.len();
        let mut maker_vols = Vec::with_capacity(n);
        let mut base_targets = Vec::with_capacity(n);
        let mut current = Vec::with_capacity(n);
        let ours_at_price = |price_cents: i16| -> f64 {
            slots
                .iter()
                .find(|s| s.yes_price_cents == price_cents)
                .map(|s| s.current_count as f64)
                .unwrap_or(0.0)
        };
        for &idx in &active {
            let slot = &slots[idx];
            let maker_vol = {
                let from_book = maker_vol_at_price(book, slot.yes_price_cents);
                if from_book > 0.0 {
                    from_book
                } else {
                    slot.initial_maker_vol
                }
            };
            maker_vols.push(maker_vol);
            current.push(slot.current_count);
        }
        let mut sum_prev = 0.0f64;
        for i in 0..n {
            let idx = active[i];
            let slot = &slots[idx];
            let maker_vol = maker_vols[i];
            let our_resting = current[i] as f64;
            let available_taker = (slot.initial_taker_vol * 0.75 - sum_prev).max(0.0);
            let price_below = if is_bid {
                slot.yes_price_cents.saturating_sub(1)
            } else {
                (slot.yes_price_cents + 1).min(99)
            };
            let vol_below = maker_vol_at_price(book, price_below);
            let ours_below = ours_at_price(price_below);
            let vol_below_net = (vol_below - ours_below).max(0.0);
            let available_maker = if i == 0 {
                vol_below_net / 2.0
            } else if vol_below_net > 0.0 {
                vol_below_net
            } else {
                (maker_vol - our_resting).max(0.0)
            };
            let base = available_taker.min(available_maker).floor().max(0.0) as i32;
            base_targets.push(base);
            sum_prev += base as f64;
        }

        let mut targets = base_targets.clone();

        let mut freed = 0i32;
        for i in 0..n {
            if base_targets[i] < current[i] {
                freed += current[i] - base_targets[i];
            }
        }
        for i in 1..n {
            if freed <= 0 {
                break;
            }
            let idx = active[i];
            let slot = &slots[idx];
            let sum_prev_i: f64 = targets[0..i].iter().map(|&t| t as f64).sum();
            let available_taker = (slot.initial_taker_vol * 0.75 - sum_prev_i).max(0.0);
            let price_below = if is_bid {
                slot.yes_price_cents.saturating_sub(1)
            } else {
                (slot.yes_price_cents + 1).min(99)
            };
            let vol_below = maker_vol_at_price(book, price_below);
            let ours_below = ours_at_price(price_below);
            let available_maker = (vol_below - ours_below).max(0.0);
            let room = (available_taker.min(available_maker).floor() as i32 - targets[i]).max(0);
            let add = room.min(freed);
            if add > 0 {
                targets[i] += add;
                freed -= add;
            }
        }
        if freed > 0 {
            eprintln!(
                "[{tag}] recalc {action}: {freed} freed vol, all lower cascades full (new levels via TouchCascadePivot only)",
            );
        }

        let mut total_deficit = 0i32;
        for i in 0..n {
            if base_targets[i] > targets[i] {
                total_deficit += base_targets[i] - targets[i];
            }
        }
        if total_deficit > 0 && n > 1 {
            let lowest_idx = n - 1;
            let take = targets[lowest_idx].min(total_deficit);
            if take > 0 {
                targets[lowest_idx] -= take;
                let mut to_distribute = take;
                for i in 0..lowest_idx {
                    if to_distribute <= 0 {
                        break;
                    }
                    let need = (base_targets[i] - targets[i]).max(0);
                    let add = need.min(to_distribute);
                    if add > 0 {
                        targets[i] += add;
                        to_distribute -= add;
                    }
                }
            }
        }

        let mut cost: f64 = 0.0;
        for (i, &idx) in active.iter().enumerate() {
            let slot = &slots[idx];
            let price_dollars = if is_bid {
                (slot.yes_price_cents as f64) / 100.0
            } else {
                (100.0 - slot.yes_price_cents as f64) / 100.0
            };
            cost += targets[i] as f64 * price_dollars;
        }
        while cost > side_budget && cost > 0.0 {
            let lowest_idx = n - 1;
            if targets[lowest_idx] <= 0 {
                break;
            }
            let idx = active[lowest_idx];
            let slot = &slots[idx];
            let price_dollars = if is_bid {
                (slot.yes_price_cents as f64) / 100.0
            } else {
                (100.0 - slot.yes_price_cents as f64) / 100.0
            };
            let reduce = ((cost - side_budget) / price_dollars).ceil().max(1.0) as i32;
            let take = targets[lowest_idx].min(reduce);
            targets[lowest_idx] -= take;
            cost -= take as f64 * price_dollars;
        }

        for (i, &idx) in active.iter().enumerate() {
            let slot = &mut slots[idx];
            let new_count = targets[i];
            if new_count <= 0 && slot.current_count > 0 {
                if ops.cancel_slot(slot) {
                    eprintln!(
                        "[{tag}] recalc cancelled {action} at {}¢ (maker vol {:.0} - ours {})",
                        slot.yes_price_cents, maker_vols[i], slot.current_count
                    );
                    slot.current_count = 0;
                }
            } else if new_count != slot.current_count && new_count > 0 {
                let yes_price = slot.yes_price_cents;
                let prev_count = slot.current_count;
                if ops.resize_slot(slot, action, new_count).is_ok() {
                    eprintln!(
                        "[{tag}] recalc amended {action} at {}¢: {} → {} (maker_vol={:.0})",
                        yes_price, prev_count, new_count, maker_vols[i]
                    );
                }
            }
        }
    };
    process_side(bid_slots, k_bids, true);
    process_side(ask_slots, k_asks, false);
}

pub fn apply_poly_vol_to_one_slot<O: MakerAmendOps>(
    ops: &O,
    slot: &mut MakerOrderSlot,
    new_vol: f64,
    action: &str,
) -> Option<i32> {
    let tag = ops.log_tag();
    let orig = slot.original_count as f64;
    if orig <= 0.0 {
        return None;
    }

    let new_count = if new_vol <= 0.0 || slot.initial_taker_vol <= 0.0 {
        0
    } else {
        let ratio = new_vol / slot.initial_taker_vol;
        (orig * ratio).floor().max(0.0) as i32
    };

    let old_count = slot.current_count;
    if new_count <= 0 && slot.current_count > 0 {
        if ops.cancel_slot(slot) {
            eprintln!(
                "[{tag}] cancelled {action} at {}c (vol→0)",
                slot.yes_price_cents
            );
            slot.current_count = 0;
            slot.original_count = 0;
            slot.initial_taker_vol = new_vol;
            return Some(old_count);
        }
    } else if new_count != slot.current_count && new_count > 0 {
        let yes_price = slot.yes_price_cents;
        let prev_count = slot.current_count;
        match ops.resize_slot(slot, action, new_count) {
            MakerResizeOutcome::Ok => {
                eprintln!(
                    "[{tag}] amended {action} at {}c: {} → {}",
                    yes_price, prev_count, new_count
                );
                slot.initial_taker_vol = new_vol;
                slot.original_count = slot.current_count;
                return Some(old_count);
            }
            MakerResizeOutcome::LiveAfterDownsizeRetry
                if slot.has_primary_order() && slot.current_count > 0 =>
            {
                // Peer vol reflected a smaller target; we could not open that size but restored prior rest.
                slot.initial_taker_vol = new_vol;
                slot.original_count = slot.current_count;
            }
            MakerResizeOutcome::FailedNoChange => {}
            MakerResizeOutcome::LiveAfterDownsizeRetry => {}
        }
    } else if new_count == slot.current_count {
        slot.initial_taker_vol = new_vol;
        slot.original_count = slot.current_count;
    }
    None
}

pub fn handle_volume_update<O: MakerAmendOps>(
    ops: &O,
    update: &TakerLevelVolUpdatePayload,
    bid_slots: &mut Vec<MakerOrderSlot>,
    ask_slots: &mut Vec<MakerOrderSlot>,
    db: &Option<ArbDb>,
    market: &str,
    maker_kb: &[PriceLevel],
    maker_ka: &[PriceLevel],
    kalshi_balance: f64,
    poly_balance: f64,
    side_cap: f64,
) {
    if let Some(ref poly_book) = update.taker_book {
        handle_volume_update_build_cascade(
            ops,
            maker_kb,
            maker_ka,
            poly_book,
            bid_slots,
            ask_slots,
            db,
            market,
            kalshi_balance,
            poly_balance,
            side_cap,
        );
    } else {
        let (action, is_bid) = if update.side == Side::Bid {
            ("buy", true)
        } else {
            ("sell", false)
        };
        for (price_cents, new_vol) in &update.levels {
            let old_count = if is_bid {
                bid_slots
                    .iter_mut()
                    .find(|s| s.level_price_cents == *price_cents)
                    .and_then(|slot| apply_poly_vol_to_one_slot(ops, slot, *new_vol, action))
            } else {
                ask_slots
                    .iter_mut()
                    .find(|s| s.level_price_cents == *price_cents)
                    .and_then(|slot| apply_poly_vol_to_one_slot(ops, slot, *new_vol, action))
            };
            if let Some(old_count) = old_count {
                if let Some(db) = db {
                    let cascade = maker_cascade_overlay(bid_slots, ask_slots);
                    log_db(
                        &format!("{}.record_resize", ops.log_tag()),
                        db.record_resize(
                            &[],
                            &[],
                            &[],
                            &[],
                            *price_cents as i32,
                            is_bid,
                            old_count as f64,
                            *new_vol,
                            Some(&cascade),
                        ),
                    );
                }
            }
        }
    }
}

fn reanchor_build_cascade_slot_metadata(
    slot: &mut MakerOrderSlot,
    target: i32,
    live_after_downsize_retry: bool,
    taker_vol: f64,
    maker_vol: f64,
) {
    if slot.current_count == target || live_after_downsize_retry {
        slot.initial_taker_vol = taker_vol;
        slot.initial_maker_vol = maker_vol;
        slot.original_count = slot.current_count;
    }
}

pub fn handle_volume_update_build_cascade<O: MakerAmendOps>(
    ops: &O,
    maker_kb: &[PriceLevel],
    maker_ka: &[PriceLevel],
    taker_book: &PolyFullBookPayload,
    bid_slots: &mut Vec<MakerOrderSlot>,
    ask_slots: &mut Vec<MakerOrderSlot>,
    db: &Option<ArbDb>,
    market: &str,
    kalshi_balance: f64,
    poly_balance: f64,
    side_cap: f64,
) {
    let tag = ops.log_tag();
    let mut st = MakerState::init();
    st.kalshi_balance = kalshi_balance;
    st.poly_balance = poly_balance;
    st.side_cap = side_cap;
    st.yes_bids = maker_kb.to_vec();
    st.yes_asks = maker_ka.to_vec();
    st.yes_bids.sort_by(|a, b| {
        b.price
            .partial_cmp(&a.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    st.yes_asks.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let cascade = build_cascade(&st, taker_book);

    let bid_targets: HashMap<i16, i32> = cascade
        .levels
        .bid_levels
        .iter()
        .map(|(k, v)| (*k, v.floor().max(1.0) as i32))
        .collect();
    let ask_targets: HashMap<i16, i32> = cascade
        .levels
        .ask_levels
        .iter()
        .map(|(k, v)| (*k, v.floor().max(1.0) as i32))
        .collect();

    let apply_side =
        |slots: &mut [MakerOrderSlot], targets: &HashMap<i16, i32>, action: &str| -> Vec<i16> {
            let mut live_after_downsize_retry_levels = Vec::new();
            let mut decreases: Vec<(usize, i32)> = Vec::new();
            let mut increases: Vec<(usize, i32)> = Vec::new();
            for (i, slot) in slots.iter().enumerate() {
                if !slot.has_primary_order() {
                    continue;
                }
                let target = targets.get(&slot.level_price_cents).copied().unwrap_or(0);
                if target <= 0 && slot.current_count > 0 {
                    decreases.push((i, 0));
                } else if target < slot.current_count {
                    decreases.push((i, target));
                } else if target > slot.current_count {
                    increases.push((i, target));
                }
            }
            for (i, target) in decreases {
                let slot = &mut slots[i];
                if target <= 0 {
                    if ops.cancel_slot(slot) {
                        eprintln!(
                            "[{tag}] build_cascade cancel {action} at {}¢",
                            slot.yes_price_cents
                        );
                        slot.current_count = 0;
                        slot.initial_taker_vol = 0.0;
                    }
                } else {
                    let yes_price = slot.yes_price_cents;
                    let prev_count = slot.current_count;
                    match ops.resize_slot(slot, action, target) {
                        MakerResizeOutcome::Ok => {
                            eprintln!(
                                "[{tag}] build_cascade amend {action} at {}¢: {} → {} (decrease)",
                                yes_price, prev_count, target
                            );
                        }
                        MakerResizeOutcome::LiveAfterDownsizeRetry
                            if slot.has_primary_order() && slot.current_count > 0 =>
                        {
                            eprintln!(
                                "[{tag}] build_cascade restore {action} at {}¢: target {}, live {}",
                                yes_price, target, slot.current_count
                            );
                            live_after_downsize_retry_levels.push(slot.level_price_cents);
                        }
                        MakerResizeOutcome::FailedNoChange
                        | MakerResizeOutcome::LiveAfterDownsizeRetry => {}
                    }
                }
            }
            for (i, target) in increases {
                let slot = &mut slots[i];
                let yes_price = slot.yes_price_cents;
                let prev_count = slot.current_count;
                if ops.resize_slot(slot, action, target).is_ok() {
                    eprintln!(
                        "[{tag}] build_cascade amend {action} at {}¢: {} → {} (increase)",
                        yes_price, prev_count, target
                    );
                }
            }
            live_after_downsize_retry_levels
        };

    let bid_live_after_downsize_retry_levels = apply_side(bid_slots, &bid_targets, "buy");
    let ask_live_after_downsize_retry_levels = apply_side(ask_slots, &ask_targets, "sell");

    for (i, (k_bid, _)) in cascade.levels.bid_levels.iter().enumerate() {
        if let Some(slot) = bid_slots.iter_mut().find(|s| s.level_price_cents == *k_bid) {
            let target = *bid_targets.get(k_bid).unwrap_or(&0);
            reanchor_build_cascade_slot_metadata(
                slot,
                target,
                bid_live_after_downsize_retry_levels.contains(k_bid),
                cascade.bid_taker_vols.get(i).copied().unwrap_or(0.0),
                cascade.bid_maker_vols.get(i).copied().unwrap_or(0.0),
            );
        }
    }
    for (i, (level_cents, _)) in cascade.levels.ask_levels.iter().enumerate() {
        if let Some(slot) = ask_slots
            .iter_mut()
            .find(|s| s.level_price_cents == *level_cents)
        {
            let target = *ask_targets.get(level_cents).unwrap_or(&0);
            reanchor_build_cascade_slot_metadata(
                slot,
                target,
                ask_live_after_downsize_retry_levels.contains(level_cents),
                cascade.ask_taker_vols.get(i).copied().unwrap_or(0.0),
                cascade.ask_maker_vols.get(i).copied().unwrap_or(0.0),
            );
        }
    }
    let _ = (db, market);
}

pub fn handle_level_update_ipc<O: MakerAmendOps>(
    ops: &O,
    u: &LevelUpdate,
    bid_slots: &mut [MakerOrderSlot],
    ask_slots: &mut [MakerOrderSlot],
    _db: &Option<ArbDb>,
    _market: &str,
) {
    let tag = ops.log_tag();
    for item in &u.updates {
        let slots_bid = bid_slots
            .iter_mut()
            .find(|s| s.level_price_cents == item.level_price_cents);
        let slots_ask = ask_slots
            .iter_mut()
            .find(|s| s.level_price_cents == item.level_price_cents);
        let (slot, action) = if let Some(s) = slots_bid {
            (s, "buy")
        } else if let Some(s) = slots_ask {
            (s, "sell")
        } else {
            continue;
        };

        match item.action {
            LevelAction::Cancel => {
                if slot.current_count > 0 && ops.cancel_slot(slot) {
                    eprintln!(
                        "[{tag}] LevelUpdate cancel at level {}c",
                        item.level_price_cents
                    );
                    slot.current_count = 0;
                }
            }
            LevelAction::Amend => {
                if let Some(nq) = item.new_qty {
                    let nc = nq as i32;
                    if nc <= 0 {
                        if ops.cancel_slot(slot) {
                            slot.current_count = 0;
                        }
                    } else if nc != slot.current_count {
                        if ops.resize_slot(slot, action, nc).is_ok() {
                            // current_count set inside amend impl
                        }
                    }
                }
            }
        }
    }
}

// --- Unified maker outer loop ---

pub trait MakerVenueOps: MakerAmendOps {
    fn market(&self) -> &str;
    fn snapshot_book_for_ipc(&mut self) -> MakerBookSnapshot;
    fn ws_book_cents(&mut self) -> (Vec<(f64, f64)>, Vec<(f64, f64)>);
    fn ws_book_as_yes_levels(&mut self) -> (Vec<PriceLevel>, Vec<PriceLevel>);
    fn ws_service(&mut self, ms: u64);
    fn ws_done(&self) -> bool;
    fn subscribe_fills(&mut self);
    fn batch_place_initial(
        &mut self,
        orders: &[CascadeOrder],
        taker_book: &PolyFullBookPayload,
        db: &Option<ArbDb>,
    ) -> anyhow::Result<Vec<String>>;
    fn touch_cascade_pivot(
        &mut self,
        pivot: &TouchCascadePivot,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        db: &Option<ArbDb>,
    ) -> anyhow::Result<()>;
    fn poll_fill_events(
        &mut self,
        bid_slots: &mut Vec<MakerOrderSlot>,
        ask_slots: &mut Vec<MakerOrderSlot>,
        db: &Option<ArbDb>,
    ) -> Vec<MakerFillPayload>;
    /// Kalshi-balance input to `build_cascade` / recalc (env override or venue default).
    fn recalc_balance_kalshi_leg(&mut self) -> f64;
    /// Poly-balance input to `build_cascade` / recalc (env override or venue default).
    fn recalc_balance_poly_leg(&mut self) -> f64;
    fn db_log_tag(&self) -> &'static str;
    fn cancel_every_slot_best_effort(
        &mut self,
        bid_slots: &mut [MakerOrderSlot],
        ask_slots: &mut [MakerOrderSlot],
    );
    /// Kalshi book + Poly book slices for `record_abort` (formats match each process's prior logging).
    fn books_for_abort_log(
        &mut self,
    ) -> (
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
        Vec<PriceLevel>,
    );
}

/// Shared outer maker loop for Kalshi-as-maker and Poly-as-maker children.
pub fn maker_run<O: MakerVenueOps>(
    ops: &mut O,
    fd_in: RawFd,
    fd_out: RawFd,
    creds: &ArbCreds,
) -> anyhow::Result<()> {
    let db = ArbDb::open(creds).ok();
    let snap = ops.snapshot_book_for_ipc();
    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::MakerBookSnapshot(snap)) {
        return Err(e).context(format!("{} send MakerBookSnapshot", ops.log_tag()));
    }

    let cascade_msg = match arb_ipc::recv_msg(fd_in)? {
        ArbMsg::CascadeOrders(c) => c,
        ArbMsg::AbortFatal(a) => {
            eprintln!("[{}] peer fatal: {}", ops.log_tag(), a.message);
            return Ok(());
        }
        ArbMsg::Abort { reason } | ArbMsg::MakerAbort { reason } => {
            eprintln!("[{}] peer abort: {reason}", ops.log_tag());
            return Ok(());
        }
        other => anyhow::bail!("[{}] expected CascadeOrders got {other:?}", ops.log_tag()),
    };

    let market = ops.market().to_string();

    let poly_at_cascade = cascade_msg.taker_book.clone().unwrap_or_default();
    let (init_mb, init_ma) = ops.ws_book_as_yes_levels();
    if let Some(db) = &db {
        log_db(
            &format!("{}.record_start", ops.db_log_tag()),
            db.record_start(
                &market,
                &init_mb,
                &init_ma,
                &poly_at_cascade.bids,
                &poly_at_cascade.asks,
                None,
            ),
        );
    }

    if cascade_msg.orders.is_empty() {
        eprintln!("[{}] empty cascade", ops.log_tag());
        arb_ipc::send_msg_eprint(
            fd_out,
            &ArbMsg::AbortFatal(AbortFatalPayload {
                version: IPC_VERSION,
                ts: now_ms(),
                reason_code: "no_orders".into(),
                message: "empty CascadeOrders".into(),
            }),
            &format!("{}.AbortFatal", ops.log_tag()),
        );
        return Ok(());
    }

    let ids = match ops.batch_place_initial(&cascade_msg.orders, &poly_at_cascade, &db) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[{}] initial batch place: {e:#}", ops.log_tag());
            arb_ipc::send_msg_eprint(
                fd_out,
                &ArbMsg::AbortFatal(AbortFatalPayload {
                    version: IPC_VERSION,
                    ts: now_ms(),
                    reason_code: "place_failed".into(),
                    message: e.to_string(),
                }),
                &format!("{}.AbortFatal", ops.log_tag()),
            );
            return Ok(());
        }
    };

    let (mut bid_slots, mut ask_slots) = match orders_to_maker_slots(&cascade_msg.orders, &ids) {
        Ok(slots) => slots,
        Err(e) => {
            eprintln!("[{}] slot bookkeeping after place: {e}", ops.log_tag());
            for id in &ids {
                if !id.is_empty() {
                    let _ = ops.cancel_one(id);
                }
            }
            return Err(e);
        }
    };

    let levels_done = MakerLevelsDonePayload {
        bid_levels: bid_slots
            .iter()
            .map(|s| (s.level_price_cents, s.current_count as f64))
            .collect(),
        ask_levels: ask_slots
            .iter()
            .map(|s| (s.level_price_cents, s.current_count as f64))
            .collect(),
    };
    if let Err(e) = arb_ipc::send_msg(fd_out, &ArbMsg::MakerLevelsDone(levels_done)) {
        eprintln!("[{}] send levels: {e:#}", ops.log_tag());
        ops.cancel_every_slot_best_effort(&mut bid_slots, &mut ask_slots);
        return Err(e).context(format!("{} send levels", ops.log_tag()));
    }

    ops.subscribe_fills();

    let (mut maker_kb_cents, mut maker_ka_cents) = ops.ws_book_cents();
    let mut last_market_bid: Option<i16> = best_yes_bid_touch(&maker_kb_cents).map(|(p, _)| p);
    let mut last_market_ask: Option<i16> = best_yes_ask_touch(&maker_ka_cents).map(|(p, _)| p);

    let mut position: i64 = 0;
    let max_iterations = 12000;
    let mut abort_reason: Option<String> = None;
    let mut last_recalc_u64: u64 = now_ms();

    for _ in 0..max_iterations {
        if shutdown::shutdown_requested() {
            eprintln!("[{}] exit: user interrupt (Ctrl+C)", ops.log_tag());
            abort_reason = Some("user_interrupt".to_string());
            break;
        }
        if ops.ws_done() {
            eprintln!("[{}] exit: WebSocket disconnected", ops.log_tag());
            abort_reason = Some(format!("{}_ws_disconnected", ops.log_tag()));
            break;
        }

        let ipc_ready = match arb_ipc::poll_readable(fd_in, 50) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[{}] poll_readable: {e:#}", ops.log_tag());
                false
            }
        };
        if ipc_ready {
            let m: ArbMsg = match arb_ipc::recv_msg(fd_in) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[{}] IPC recv failed: {e:#}", ops.log_tag());
                    abort_reason = Some(format!("ipc_recv:{e}"));
                    break;
                }
            };
            match m {
                ArbMsg::Abort { reason } | ArbMsg::MakerAbort { reason } => {
                    eprintln!("[{}] exit request from peer: {reason}", ops.log_tag());
                    abort_reason = Some(reason);
                    break;
                }
                ArbMsg::AbortFatal(a) => {
                    eprintln!(
                        "[{}] exit (fatal) from peer: {} — {}",
                        ops.log_tag(),
                        a.reason_code,
                        a.message
                    );
                    abort_reason = Some(format!("{}:{}", a.reason_code, a.message));
                    break;
                }
                ArbMsg::TakerFullBook(_) => {}
                ArbMsg::TakerLevelVolUpdate(u) => {
                    let kalshi_balance = std::env::var("ARB_KALSHI_BALANCE")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| ops.recalc_balance_kalshi_leg());
                    let poly_balance = std::env::var("ARB_POLY_BALANCE")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| ops.recalc_balance_poly_leg());
                    let side_cap = std::env::var("ARB_SIDE_CAP")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1000.0);
                    let (maker_kb, maker_ka) = ops.ws_book_as_yes_levels();
                    let (mk_cents_b, mk_cents_a) = ops.ws_book_cents();
                    maker_kb_cents = mk_cents_b;
                    maker_ka_cents = mk_cents_a;
                    let used_build_cascade = u.taker_book.is_some();
                    handle_volume_update(
                        ops,
                        &u,
                        &mut bid_slots,
                        &mut ask_slots,
                        &db,
                        &market,
                        &maker_kb,
                        &maker_ka,
                        kalshi_balance,
                        poly_balance,
                        side_cap,
                    );
                    if !used_build_cascade {
                        recalc_cascade_volumes(
                            ops,
                            &mut bid_slots,
                            &mut ask_slots,
                            &maker_kb_cents,
                            &maker_ka_cents,
                            kalshi_balance,
                            poly_balance,
                            side_cap,
                        );
                    }
                }
                ArbMsg::LevelUpdate(u) => {
                    handle_level_update_ipc(ops, &u, &mut bid_slots, &mut ask_slots, &db, &market);
                }
                ArbMsg::TouchCascadePivot(p) => {
                    if let Err(e) = ops.touch_cascade_pivot(&p, &mut bid_slots, &mut ask_slots, &db)
                    {
                        eprintln!("[{}] touch pivot failed: {e}", ops.log_tag());
                        arb_ipc::send_msg_eprint(
                            fd_out,
                            &ArbMsg::AbortFatal(AbortFatalPayload {
                                version: IPC_VERSION,
                                ts: now_ms(),
                                reason_code: "touch_pivot_failed".into(),
                                message: e.to_string(),
                            }),
                            &format!("{}.AbortFatal", ops.log_tag()),
                        );
                        abort_reason = Some(format!("touch_pivot_failed:{e}"));
                        break;
                    }
                }
                _ => {}
            }
        }

        ops.ws_service(50);

        let (mk_b_now, mk_a_now) = ops.ws_book_cents();
        maker_kb_cents = mk_b_now;
        maker_ka_cents = mk_a_now;

        if now_ms().saturating_sub(last_recalc_u64) >= 5000 {
            let kalshi_balance = std::env::var("ARB_KALSHI_BALANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| ops.recalc_balance_kalshi_leg());
            let poly_balance = std::env::var("ARB_POLY_BALANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| ops.recalc_balance_poly_leg());
            let side_cap = std::env::var("ARB_SIDE_CAP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000.0);
            recalc_cascade_volumes(
                ops,
                &mut bid_slots,
                &mut ask_slots,
                &maker_kb_cents,
                &maker_ka_cents,
                kalshi_balance,
                poly_balance,
                side_cap,
            );
            last_recalc_u64 = now_ms();
        }

        if let Some((p, q)) = best_yes_bid_touch(&maker_kb_cents) {
            if last_market_bid != Some(p) {
                if bid_touch_moved_only_by_own_limit(last_market_bid, p, &bid_slots) {
                    if let Some(s) = top_active_bid_slot(&bid_slots) {
                        eprintln!(
                            "[{}] skip bid touch notify: {:?}→{}¢ (our tier {}¢ limit {}¢)",
                            ops.log_tag(),
                            last_market_bid,
                            p,
                            s.level_price_cents,
                            s.yes_price_cents
                        );
                    }
                } else {
                    arb_ipc::send_msg_eprint(
                        fd_out,
                        &ArbMsg::MakerTouchChanged(MakerTouchChanged {
                            version: IPC_VERSION,
                            ts: now_ms(),
                            side: Side::Bid,
                            new_k_cents: p,
                            new_k_qty: q,
                        }),
                        &format!("{}.MakerTouchChanged.bid", ops.log_tag()),
                    );
                }
                last_market_bid = Some(p);
            }
        } else {
            last_market_bid = None;
        }
        if let Some((p, q)) = best_yes_ask_touch(&maker_ka_cents) {
            if last_market_ask != Some(p) {
                if ask_touch_moved_only_by_own_limit(last_market_ask, p, &ask_slots) {
                    if let Some(s) = top_active_ask_slot(&ask_slots) {
                        eprintln!(
                            "[{}] skip ask touch notify: {:?}→{}¢ (our level {}¢ limit {}¢, was k_ask {}¢)",
                            ops.log_tag(),
                            last_market_ask,
                            p,
                            s.level_price_cents,
                            s.yes_price_cents,
                            s.level_price_cents.saturating_add(1),
                        );
                    }
                } else {
                    arb_ipc::send_msg_eprint(
                        fd_out,
                        &ArbMsg::MakerTouchChanged(MakerTouchChanged {
                            version: IPC_VERSION,
                            ts: now_ms(),
                            side: Side::Ask,
                            new_k_cents: p,
                            new_k_qty: q,
                        }),
                        &format!("{}.MakerTouchChanged.ask", ops.log_tag()),
                    );
                }
                last_market_ask = Some(p);
            }
        } else {
            last_market_ask = None;
        }

        let fills = ops.poll_fill_events(&mut bid_slots, &mut ask_slots, &db);
        for fill in fills {
            if fill.side == Side::Bid {
                position += fill.filled_count as i64;
            } else {
                position -= fill.filled_count as i64;
            }
            arb_ipc::send_msg_eprint(
                fd_out,
                &ArbMsg::MakerFill(fill),
                &format!("{}.MakerFill", ops.log_tag()),
            );
        }
    }

    let cascade_before_shutdown = maker_cascade_overlay(&bid_slots, &ask_slots);
    ops.cancel_every_slot_best_effort(&mut bid_slots, &mut ask_slots);

    if let Some(reason) = &abort_reason {
        let (book_kb, book_ka, book_pb, book_pa) = ops.books_for_abort_log();
        if let Some(db) = &db {
            log_db(
                &format!("{}.record_abort", ops.db_log_tag()),
                db.record_abort(
                    &market,
                    reason,
                    &book_kb,
                    &book_ka,
                    &book_pb,
                    &book_pa,
                    Some(&cascade_before_shutdown),
                ),
            );
        }
        arb_ipc::send_msg_eprint(
            fd_out,
            &ArbMsg::MakerAbort {
                reason: reason.clone(),
            },
            &format!("{}.MakerAbort", ops.log_tag()),
        );
    }

    eprintln!("[{}] done, final position: {position}", ops.log_tag());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_slot() -> MakerOrderSlot {
        MakerOrderSlot {
            order_id: "primary".into(),
            level_price_cents: 50,
            yes_price_cents: 51,
            original_count: 20,
            current_count: 10,
            initial_taker_vol: 100.0,
            initial_maker_vol: 500.0,
            child_orders: Vec::new(),
            last_matched_reported: 0.0,
        }
    }

    #[test]
    fn build_cascade_metadata_reanchors_restored_live_slot() {
        let mut slot = test_slot();
        reanchor_build_cascade_slot_metadata(&mut slot, 6, true, 70.0, 400.0);

        assert!((slot.initial_taker_vol - 70.0).abs() < 1e-9);
        assert!((slot.initial_maker_vol - 400.0).abs() < 1e-9);
        assert_eq!(slot.original_count, 10);
    }

    #[test]
    fn build_cascade_metadata_ignores_unchanged_failed_slot() {
        let mut slot = test_slot();
        reanchor_build_cascade_slot_metadata(&mut slot, 6, false, 70.0, 400.0);

        assert!((slot.initial_taker_vol - 100.0).abs() < 1e-9);
        assert!((slot.initial_maker_vol - 500.0).abs() < 1e-9);
        assert_eq!(slot.original_count, 20);
    }
}
