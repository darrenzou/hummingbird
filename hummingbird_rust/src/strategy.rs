//! Cascade sizing: how much to rest on the maker given the taker book.
//!
//! For each maker YES bid/ask rung, size is the min of:
//! - half the maker size at that price
//! - 75% of remaining taker depth beyond the edge
//! - leftover per-side budget (`min(half portfolio, side_cap)`)
//!
//! A rung is skipped unless the maker limit sits at least [`EDGE_CENTS`] inside
//! the taker best bid/ask. See [`build_cascade`].

use crate::poly_live::round_poly_price;
use crate::types::{
    CascadeOrder, CascadeOrders, MakerLevelsDonePayload, PolyFullBookPayload, PriceLevel, Side,
    ARB_MAX_TRACKED, IPC_VERSION,
};

/// Cross-venue edge: maker resting limit must be at least this many **cents** inside the taker BBO.
pub const EDGE_CENTS: i16 = 2;

/// Accumulates signed hedge demand on the **taker** venue. Call after each maker fill.
/// Conventions: **+n** = buy `n` on taker; **-n** = sell `n` on taker.
/// Maker bid fill → maker bought YES → taker hedges by selling YES → subtract.
/// Maker ask fill → taker buys YES → add.
#[inline]
pub fn hedge_accum_apply(acc: &mut i32, maker_fill_side: Side, filled_count: u32) {
    match maker_fill_side {
        Side::Bid => *acc -= filled_count as i32,
        Side::Ask => *acc += filled_count as i32,
    }
}

/// Maker book + cash used by [`build_cascade`].
#[derive(Debug, Clone, Default)]
pub struct MakerState {
    pub yes_bids: Vec<PriceLevel>,
    pub yes_asks: Vec<PriceLevel>,
    pub kalshi_balance: f64,
    pub poly_balance: f64,
    pub side_cap: f64,
}

impl MakerState {
    pub fn init() -> Self {
        Self {
            side_cap: 1000.0,
            ..Default::default()
        }
    }
}

fn poly_best_bid(book: &PolyFullBookPayload) -> f64 {
    book.bids
        .first()
        .map(|l| round_poly_price(l.price))
        .unwrap_or(0.0)
}
fn poly_best_ask(book: &PolyFullBookPayload) -> f64 {
    book.asks
        .first()
        .map(|l| round_poly_price(l.price))
        .unwrap_or(1.0)
}

fn poly_vol_above(book: &PolyFullBookPayload, price_cents: i16) -> f64 {
    let thr = round_poly_price((price_cents as f64) / 100.0);
    book.bids
        .iter()
        .filter(|l| round_poly_price(l.price) > thr)
        .map(|l| l.size)
        .sum()
}
fn poly_vol_below(book: &PolyFullBookPayload, price_cents: i16) -> f64 {
    let thr = round_poly_price((price_cents as f64) / 100.0);
    book.asks
        .iter()
        .filter(|l| round_poly_price(l.price) < thr)
        .map(|l| l.size)
        .sum()
}

#[derive(Debug, Clone)]
pub struct CascadeResult {
    pub levels: MakerLevelsDonePayload,
    pub bid_taker_vols: Vec<f64>,
    pub ask_taker_vols: Vec<f64>,
    pub bid_maker_vols: Vec<f64>,
    pub ask_maker_vols: Vec<f64>,
}

/// Cascade sizing using maker ladder + taker liquidity (taker book in US dollars / outcome).
/// Bid side: maker limit `(k_bid + 1).min(99)¢` must satisfy `limit + EDGE_CENTS <= taker_best_bid`.
/// Ask side: maker touch `k_ask` must satisfy `k_ask > taker_ask + EDGE_CENTS`; cascade **level** is `(k_ask - 1)¢`.
pub fn build_cascade(st: &MakerState, taker_book: &PolyFullBookPayload) -> CascadeResult {
    let mut out = MakerLevelsDonePayload::default();
    let mut bid_taker_vols = Vec::new();
    let mut ask_taker_vols = Vec::new();
    let mut bid_maker_vols = Vec::new();
    let mut ask_maker_vols = Vec::new();

    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let mut bid_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));
    let mut ask_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));

    let p_bid = poly_best_bid(taker_book) * 100.0;
    let p_ask = poly_best_ask(taker_book) * 100.0;

    let mut sum_prev = 0.0;
    for lvl in &st.yes_bids {
        if out.bid_levels.len() >= ARB_MAX_TRACKED {
            break;
        }
        let k_bid = (lvl.price + 0.5) as i16;
        let k_qty = lvl.size;
        let limit_cents = cascade_bid_limit_price(k_bid);
        if (limit_cents as f64) + EDGE_CENTS as f64 > p_bid {
            continue;
        }
        let poly_vol = poly_vol_above(taker_book, k_bid);
        let available_poly = poly_vol * 0.75 - sum_prev;
        let available_kalshi = k_qty;
        let available = available_poly.min(available_kalshi);
        if available <= 0.0 {
            break;
        }
        let mut requested = (k_qty / 2.0).min(available);
        if requested <= 0.0 {
            break;
        }
        let price_dollars = (limit_cents as f64) / 100.0;
        if price_dollars <= 0.0 {
            break;
        }
        let max_by_budget = bid_budget / price_dollars;
        if max_by_budget <= 0.0 {
            break;
        }
        if requested > max_by_budget {
            requested = max_by_budget;
        }
        sum_prev += requested;
        bid_budget = (bid_budget - requested * price_dollars).max(0.0);
        out.bid_levels.push((k_bid, requested));
        bid_taker_vols.push(poly_vol);
        bid_maker_vols.push(k_qty);
        if bid_budget <= 0.0 {
            break;
        }
    }

    let mut sum_prev = 0.0;
    for lvl in &st.yes_asks {
        if out.ask_levels.len() >= ARB_MAX_TRACKED {
            break;
        }
        let k_ask = (lvl.price + 0.5) as i16;
        let k_qty = lvl.size;
        if (k_ask as f64) <= p_ask + EDGE_CENTS as f64 {
            continue;
        }
        let level_cents = k_ask - 1;
        let poly_vol = poly_vol_below(taker_book, level_cents);
        let available_poly = poly_vol * 0.75 - sum_prev;
        let available_kalshi = k_qty;
        let available = available_poly.min(available_kalshi);
        if available <= 0.0 {
            break;
        }
        let mut requested = (k_qty / 2.0).min(available);
        if requested <= 0.0 {
            break;
        }
        let price_dollars = (100.0 - level_cents as f64) / 100.0;
        if price_dollars <= 0.0 {
            break;
        }
        let max_by_budget = ask_budget / price_dollars;
        if max_by_budget <= 0.0 {
            break;
        }
        if requested > max_by_budget {
            requested = max_by_budget;
        }
        sum_prev += requested;
        ask_budget = (ask_budget - requested * price_dollars).max(0.0);
        out.ask_levels.push((level_cents, requested));
        ask_taker_vols.push(poly_vol);
        ask_maker_vols.push(k_qty);
        if ask_budget <= 0.0 {
            break;
        }
    }

    CascadeResult {
        levels: out,
        bid_taker_vols,
        ask_taker_vols,
        bid_maker_vols,
        ask_maker_vols,
    }
}

pub fn cascade_bid_touch_size(
    st: &MakerState,
    taker_book: &PolyFullBookPayload,
    k_bid: i16,
    k_qty: f64,
) -> Option<(f64, f64)> {
    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let bid_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));
    let p_bid = poly_best_bid(taker_book) * 100.0;
    let limit_cents = (k_bid + 1).min(99);
    if (limit_cents as f64) + EDGE_CENTS as f64 > p_bid {
        return None;
    }
    let poly_vol = poly_vol_above(taker_book, k_bid);
    let available = poly_vol * 0.75;
    if available <= 0.0 {
        return None;
    }
    let mut requested = (k_qty / 2.0).min(available);
    if requested <= 0.0 {
        return None;
    }
    let price_dollars = (limit_cents as f64) / 100.0;
    let max_by_budget = bid_budget / price_dollars;
    if max_by_budget <= 0.0 {
        return None;
    }
    if requested > max_by_budget {
        requested = max_by_budget;
    }
    if requested <= 0.0 {
        return None;
    }
    Some((requested, poly_vol))
}

pub fn cascade_ask_touch_size(
    st: &MakerState,
    taker_book: &PolyFullBookPayload,
    k_ask: i16,
    k_qty: f64,
) -> Option<(f64, f64)> {
    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let ask_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));
    let p_ask = poly_best_ask(taker_book) * 100.0;
    if (k_ask as f64) <= p_ask + EDGE_CENTS as f64 {
        return None;
    }
    let level_cents = k_ask - 1;
    let poly_vol = poly_vol_below(taker_book, level_cents);
    let available = poly_vol * 0.75;
    if available <= 0.0 {
        return None;
    }
    let mut requested = (k_qty / 2.0).min(available);
    if requested <= 0.0 {
        return None;
    }
    let price_dollars = (100.0 - level_cents as f64) / 100.0;
    let max_by_budget = ask_budget / price_dollars;
    if max_by_budget <= 0.0 {
        return None;
    }
    if requested > max_by_budget {
        requested = max_by_budget;
    }
    if requested <= 0.0 {
        return None;
    }
    Some((requested, poly_vol))
}

pub fn cascade_empty_explanation(st: &MakerState, taker_book: &PolyFullBookPayload) -> String {
    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let p_bid = poly_best_bid(taker_book) * 100.0;
    let p_ask = poly_best_ask(taker_book) * 100.0;
    let bb = poly_best_bid(taker_book);
    let ba = poly_best_ask(taker_book);

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "portfolio = min(kalshi ${:.2}, poly ${:.2}) = ${:.2}\nTaker: {} bids (best {:.3} ~ {:.1}¢), {} asks (best {:.3} ~ {:.1}¢)\nMaker ladder: {} YES bid levels, {} YES ask levels | EDGE_CENTS={}",
        st.kalshi_balance,
        st.poly_balance,
        portfolio,
        taker_book.bids.len(),
        bb,
        p_bid,
        taker_book.asks.len(),
        ba,
        p_ask,
        st.yes_bids.len(),
        st.yes_asks.len(),
        EDGE_CENTS,
    ));

    if portfolio <= 0.0 {
        lines
            .push("• Budget is $0: each side’s half-portfolio is 0, so max_by_budget is 0.".into());
    }
    if taker_book.bids.is_empty() {
        lines.push(
            "• No taker bids → p_bid = 0¢ → bid leg needs limit below 0 (impossible).".into(),
        );
    } else {
        let any_bid_qual = st.yes_bids.iter().any(|l| {
            let k = (l.price + 0.5) as i16;
            let lim = (k + 1).min(99);
            (lim as f64) + EDGE_CENTS as f64 <= p_bid
        });
        if !any_bid_qual {
            lines.push(format!(
                "• Bid leg: need maker limit (k_bid+1)¢ + {EDGE_CENTS}¢ ≤ taker best bid ({:.1}¢); no maker level qualifies.",
                p_bid
            ));
        }
    }
    if taker_book.asks.is_empty() {
        lines.push(
            "• No taker asks → p_ask = 100¢ → ask leg needs k_ask > 100+EDGE (impossible).".into(),
        );
    } else {
        let any_ask_qual = st
            .yes_asks
            .iter()
            .any(|l| ((l.price + 0.5) as i16) as f64 > p_ask + EDGE_CENTS as f64);
        if !any_ask_qual {
            lines.push(format!(
                "• Ask leg: need maker YES ask > taker best ask + {EDGE_CENTS}¢ (>{:.1}¢); no maker level qualifies.",
                p_ask + EDGE_CENTS as f64
            ));
        }
    }

    if portfolio > 0.0 && !taker_book.bids.is_empty() && p_bid > 0.0 {
        for lvl in st.yes_bids.iter().take(5) {
            let k = (lvl.price + 0.5) as i16;
            let lim = (k + 1).min(99);
            if (lim as f64) + EDGE_CENTS as f64 > p_bid {
                continue;
            }
            let v = poly_vol_above(taker_book, k);
            if v <= 0.0 {
                lines.push(format!(
                    "• First qualifying bid level {k}¢ has taker volume above it = 0 → available ≤ 0.",
                ));
                break;
            }
        }
    }
    if portfolio > 0.0 && !taker_book.asks.is_empty() && p_ask < 100.0 {
        for lvl in st.yes_asks.iter().take(5) {
            let k = (lvl.price + 0.5) as i16;
            if (k as f64) <= p_ask + EDGE_CENTS as f64 {
                continue;
            }
            let v = poly_vol_below(taker_book, k - 1);
            if v <= 0.0 {
                lines.push(format!(
                    "• First qualifying ask (touch {k}¢, level {}¢) has taker volume below level = 0 → available ≤ 0.",
                    k - 1
                ));
                break;
            }
        }
    }

    lines.join("\n")
}

pub fn cascade_bid_limit_price(k_bid: i16) -> i16 {
    (k_bid + 1).min(99)
}

pub fn cascade_ask_limit_price(level_cents: i16) -> i16 {
    level_cents
}

pub fn cascade_result_to_orders(
    cascade: &CascadeResult,
    market: &str,
    ts: u64,
    taker_book: &PolyFullBookPayload,
) -> CascadeOrders {
    let mut orders = Vec::new();
    for (i, (k_bid, vol)) in cascade.levels.bid_levels.iter().enumerate() {
        let qty = (*vol as f64).floor().max(1.0) as u32;
        let limit = cascade_bid_limit_price(*k_bid);
        let initial_taker_vol = cascade.bid_taker_vols.get(i).copied().unwrap_or(0.0);
        let initial_maker_vol = cascade.bid_maker_vols.get(i).copied().unwrap_or(0.0);
        orders.push(CascadeOrder {
            level_price_cents: *k_bid,
            side: Side::Bid,
            limit_price_cents: limit,
            qty,
            initial_taker_vol,
            initial_maker_vol,
        });
    }
    for (i, (level_cents, vol)) in cascade.levels.ask_levels.iter().enumerate() {
        let qty = (*vol as f64).floor().max(1.0) as u32;
        let initial_taker_vol = cascade.ask_taker_vols.get(i).copied().unwrap_or(0.0);
        let initial_maker_vol = cascade.ask_maker_vols.get(i).copied().unwrap_or(0.0);
        let limit = cascade_ask_limit_price(*level_cents);
        orders.push(CascadeOrder {
            level_price_cents: *level_cents,
            side: Side::Ask,
            limit_price_cents: limit,
            qty,
            initial_taker_vol,
            initial_maker_vol,
        });
    }
    CascadeOrders {
        version: IPC_VERSION,
        ts,
        market: market.to_string(),
        orders,
        taker_book: Some(taker_book.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hedge_accum_plan_examples() {
        let mut a = 0i32;
        hedge_accum_apply(&mut a, Side::Ask, 2);
        assert_eq!(a, 2);
        hedge_accum_apply(&mut a, Side::Ask, 12);
        assert_eq!(a, 14);

        let mut b = 0i32;
        hedge_accum_apply(&mut b, Side::Ask, 2);
        hedge_accum_apply(&mut b, Side::Ask, 3);
        assert_eq!(b, 5);

        let mut c = 0i32;
        hedge_accum_apply(&mut c, Side::Ask, 2);
        hedge_accum_apply(&mut c, Side::Bid, 1);
        assert_eq!(c, 1);
    }
}
