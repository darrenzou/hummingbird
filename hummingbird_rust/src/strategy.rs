//! Poly-side cascade and volume math (Kalshi full book + Poly book).

use crate::types::{
    CascadeOrder, CascadeOrders, KalshiLevelsDonePayload, PolyFullBookPayload, PriceLevel, Side,
    ARB_MAX_TRACKED, IPC_VERSION,
};

#[derive(Debug, Clone, Default)]
pub struct KalshiState {
    pub yes_bids: Vec<PriceLevel>,
    pub yes_asks: Vec<PriceLevel>,
    pub kalshi_balance: f64,
    pub poly_balance: f64,
    pub side_cap: f64,
}

impl KalshiState {
    pub fn init() -> Self {
        Self {
            side_cap: 1000.0,
            ..Default::default()
        }
    }
}

fn poly_best_bid(book: &PolyFullBookPayload) -> f64 {
    book.bids.first().map(|l| l.price).unwrap_or(0.0)
}
fn poly_best_ask(book: &PolyFullBookPayload) -> f64 {
    book.asks.first().map(|l| l.price).unwrap_or(1.0)
}

fn poly_vol_above(book: &PolyFullBookPayload, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    book.bids
        .iter()
        .filter(|l| l.price > thr)
        .map(|l| l.size)
        .sum()
}
fn poly_vol_below(book: &PolyFullBookPayload, price_cents: i16) -> f64 {
    let thr = (price_cents as f64) / 100.0;
    book.asks
        .iter()
        .filter(|l| l.price < thr)
        .map(|l| l.size)
        .sum()
}

#[derive(Debug, Clone)]
pub struct CascadeResult {
    pub levels: KalshiLevelsDonePayload,
    pub bid_poly_vols: Vec<f64>,
    pub ask_poly_vols: Vec<f64>,
}

/// Cascade sizing on Poly using Kalshi ladder + Poly liquidity. Bid limit = k_bid+1¢ per spec.
pub fn build_cascade(st: &KalshiState, poly_book: &PolyFullBookPayload) -> CascadeResult {
    let mut out = KalshiLevelsDonePayload::default();
    let mut bid_poly_vols = Vec::new();
    let mut ask_poly_vols = Vec::new();

    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let mut bid_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));
    let mut ask_budget = (0.5 * portfolio).min(st.side_cap.max(1.0));

    let p_bid = (poly_best_bid(poly_book) * 100.0 + 0.5) as i16;
    let p_ask = (poly_best_ask(poly_book) * 100.0 + 0.5) as i16;

    let mut sum_prev = 0.0;
    for lvl in &st.yes_bids {
        if out.bid_levels.len() >= ARB_MAX_TRACKED {
            break;
        }
        let k_bid = (lvl.price + 0.5) as i16;
        let k_qty = lvl.size;
        if k_bid >= p_bid {
            continue;
        }
        let poly_vol = poly_vol_above(poly_book, k_bid);
        let available = poly_vol * 0.75 - sum_prev;
        if available <= 0.0 {
            break;
        }
        let mut requested = (k_qty / 2.0).min(available);
        if requested <= 0.0 {
            break;
        }
        let limit_cents = (k_bid + 1).min(99);
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
        bid_poly_vols.push(poly_vol);
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
        if k_ask < p_ask + 1 {
            continue;
        }
        let poly_vol = poly_vol_below(poly_book, k_ask);
        let available = poly_vol * 0.75 - sum_prev;
        if available <= 0.0 {
            break;
        }
        let mut requested = (k_qty / 2.0).min(available);
        if requested <= 0.0 {
            break;
        }
        let price_dollars = (k_ask as f64) / 100.0;
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
        out.ask_levels.push((k_ask, requested));
        ask_poly_vols.push(poly_vol);
        if ask_budget <= 0.0 {
            break;
        }
    }

    CascadeResult {
        levels: out,
        bid_poly_vols,
        ask_poly_vols,
    }
}

/// Why `build_cascade` may return no rows (for `orderbook_web` / debugging).
pub fn cascade_empty_explanation(st: &KalshiState, poly_book: &PolyFullBookPayload) -> String {
    let portfolio = st.kalshi_balance.min(st.poly_balance);
    let p_bid = (poly_best_bid(poly_book) * 100.0 + 0.5) as i16;
    let p_ask = (poly_best_ask(poly_book) * 100.0 + 0.5) as i16;
    let bb = poly_best_bid(poly_book);
    let ba = poly_best_ask(poly_book);

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "portfolio = min(kalshi ${:.2}, poly ${:.2}) = ${:.2}\nPoly: {} bids (best {:.4} ~ {}¢), {} asks (best {:.4} ~ {}¢)\nKalshi ladder: {} YES bid levels, {} YES ask levels",
        st.kalshi_balance,
        st.poly_balance,
        portfolio,
        poly_book.bids.len(),
        bb,
        p_bid,
        poly_book.asks.len(),
        ba,
        p_ask,
        st.yes_bids.len(),
        st.yes_asks.len(),
    ));

    if portfolio <= 0.0 {
        lines.push(
            "• Budget is $0: each side’s half-portfolio is 0, so max_by_budget is 0.".into(),
        );
    }
    if poly_book.bids.is_empty() {
        lines.push(
            "• No Polymarket bids → p_bid = 0¢ → bid cascade needs k_bid < 0 (impossible).".into(),
        );
    } else {
        let any_bid_qual = st
            .yes_bids
            .iter()
            .any(|l| ((l.price + 0.5) as i16) < p_bid);
        if !any_bid_qual {
            lines.push(format!(
                "• Bid leg: need Kalshi YES bid < Poly best bid ({}¢); no Kalshi level qualifies.",
                p_bid
            ));
        }
    }
    if poly_book.asks.is_empty() {
        lines.push(
            "• No Polymarket asks → p_ask = 100¢ → ask leg needs k_ask ≥ 101¢ (impossible).".into(),
        );
    } else {
        let any_ask_qual = st
            .yes_asks
            .iter()
            .any(|l| ((l.price + 0.5) as i16) >= p_ask + 1);
        if !any_ask_qual {
            lines.push(format!(
                "• Ask leg: need Kalshi YES ask ≥ Poly best ask + 1¢ (≥{}¢); no Kalshi level qualifies.",
                p_ask + 1
            ));
        }
    }

    if portfolio > 0.0 && !poly_book.bids.is_empty() && p_bid > 0 {
        for lvl in st.yes_bids.iter().take(5) {
            let k = (lvl.price + 0.5) as i16;
            if k >= p_bid {
                continue;
            }
            let v = poly_vol_above(poly_book, k);
            if v <= 0.0 {
                lines.push(format!(
                    "• First qualifying bid level {}¢ has Poly volume above it = 0 → available ≤ 0.",
                    k
                ));
                break;
            }
        }
    }
    if portfolio > 0.0 && !poly_book.asks.is_empty() && p_ask < 100 {
        for lvl in st.yes_asks.iter().take(5) {
            let k = (lvl.price + 0.5) as i16;
            if k < p_ask + 1 {
                continue;
            }
            let v = poly_vol_below(poly_book, k);
            if v <= 0.0 {
                lines.push(format!(
                    "• First qualifying ask level {}¢ has Poly volume below it = 0 → available ≤ 0.",
                    k
                ));
                break;
            }
        }
    }

    lines.join("\n")
}

pub fn cascade_result_to_orders(cascade: &CascadeResult, market: &str, ts: u64) -> CascadeOrders {
    let mut orders = Vec::new();
    for (i, (k_bid, vol)) in cascade.levels.bid_levels.iter().enumerate() {
        let qty = vol.floor().max(1.0) as u32;
        let limit = (*k_bid + 1).min(99);
        let initial_poly_vol = cascade.bid_poly_vols.get(i).copied().unwrap_or(0.0);
        orders.push(CascadeOrder {
            level_price_cents: *k_bid,
            side: Side::Bid,
            limit_price_cents: limit,
            qty,
            initial_poly_vol,
        });
    }
    for (i, (k_ask, vol)) in cascade.levels.ask_levels.iter().enumerate() {
        let qty = vol.floor().max(1.0) as u32;
        let initial_poly_vol = cascade.ask_poly_vols.get(i).copied().unwrap_or(0.0);
        orders.push(CascadeOrder {
            level_price_cents: *k_ask,
            side: Side::Ask,
            limit_price_cents: *k_ask,
            qty,
            initial_poly_vol,
        });
    }
    CascadeOrders {
        version: IPC_VERSION,
        ts,
        market: market.to_string(),
        orders,
    }
}
