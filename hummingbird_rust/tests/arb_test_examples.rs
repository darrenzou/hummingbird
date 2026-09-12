//! Loads `arb_test_examples.json` and checks book arithmetic + `build_cascade` outcomes.

use hummingbird_rust::poly_live::round_poly_price;
use hummingbird_rust::strategy::{build_cascade, MakerState};
use hummingbird_rust::types::{PolyFullBookPayload, PriceLevel};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../arb_test_examples.json");

#[derive(Debug, Deserialize)]
struct Fixture {
    defaults: FixtureDefaults,
    examples: Vec<Example>,
}

#[derive(Debug, Deserialize)]
struct FixtureDefaults {
    kalshi_balance: f64,
    poly_balance: f64,
    side_cap: f64,
}

#[derive(Debug, Deserialize)]
struct Example {
    id: String,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    label: String,
    kalshi: BookSide,
    poly: BookSide,
    #[serde(default)]
    checks: StepChecks,
}

#[derive(Debug, Deserialize)]
struct BookSide {
    bids: Vec<PriceLevel>,
    asks: Vec<PriceLevel>,
}

#[derive(Debug, Default, Deserialize)]
struct StepChecks {
    #[serde(default)]
    poly_ask_lt_kalshi_ask_plus_1: Option<bool>,
    #[serde(default)]
    poly_bid_gt_kalshi_bid_plus_1: Option<bool>,
    #[serde(default)]
    kalshi_ask_lt_poly_ask: Option<bool>,
    #[serde(default)]
    kalshi_bid_plus_1_lt_poly_bid: Option<bool>,
    #[serde(default)]
    expect_non_empty_bid_cascade: Option<bool>,
    #[serde(default)]
    expect_non_empty_ask_cascade: Option<bool>,
    #[serde(default)]
    expect_empty_ask_cascade: Option<bool>,
}

fn sort_kalshi(bids: &mut [PriceLevel], asks: &mut [PriceLevel]) {
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
}

fn sort_poly(bids: &mut [PriceLevel], asks: &mut [PriceLevel]) {
    for x in bids.iter_mut() {
        x.price = round_poly_price(x.price);
    }
    for x in asks.iter_mut() {
        x.price = round_poly_price(x.price);
    }
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
}

fn touches(
    kalshi_bids: &[PriceLevel],
    kalshi_asks: &[PriceLevel],
    poly: &PolyFullBookPayload,
) -> (i16, i16, f64, f64) {
    let k_bid = (kalshi_bids[0].price + 0.5) as i16;
    let k_ask = (kalshi_asks[0].price + 0.5) as i16;
    let p_bid = poly_best_bid_cents(poly);
    let p_ask = poly_best_ask_cents(poly);
    (k_bid, k_ask, p_bid, p_ask)
}

fn poly_best_bid_cents(book: &PolyFullBookPayload) -> f64 {
    book.bids
        .first()
        .map(|l| round_poly_price(l.price) * 100.0)
        .unwrap_or(0.0)
}

fn poly_best_ask_cents(book: &PolyFullBookPayload) -> f64 {
    book.asks
        .first()
        .map(|l| round_poly_price(l.price) * 100.0)
        .unwrap_or(100.0)
}

fn assert_checks(
    ex_id: &str,
    step_label: &str,
    k_bid: i16,
    k_ask: i16,
    p_bid: f64,
    p_ask: f64,
    c: &StepChecks,
    cascade: &hummingbird_rust::strategy::CascadeResult,
) {
    if c.poly_ask_lt_kalshi_ask_plus_1 == Some(true) {
        assert!(
            p_ask < k_ask as f64 + 1.0,
            "{} {}: want poly_ask ({}) < kalshi_ask+1 ({}+1)",
            ex_id,
            step_label,
            p_ask,
            k_ask
        );
    }
    if c.poly_bid_gt_kalshi_bid_plus_1 == Some(true) {
        assert!(
            p_bid > k_bid as f64 + 1.0,
            "{} {}: want poly_bid ({}) > kalshi_bid+1 ({}+1)",
            ex_id,
            step_label,
            p_bid,
            k_bid
        );
    }
    if c.kalshi_ask_lt_poly_ask == Some(true) {
        assert!(
            (k_ask as f64) < p_ask,
            "{} {}: want kalshi_ask ({}) < poly_ask ({})",
            ex_id,
            step_label,
            k_ask,
            p_ask
        );
    }
    if c.kalshi_bid_plus_1_lt_poly_bid == Some(true) {
        assert!(
            ((k_bid + 1) as f64) < p_bid,
            "{} {}: want kalshi_bid+1 ({}+1) < poly_bid ({})",
            ex_id,
            step_label,
            k_bid,
            p_bid
        );
    }
    if c.expect_non_empty_bid_cascade == Some(true) {
        assert!(
            !cascade.levels.bid_levels.is_empty(),
            "{} {}: expected bid cascade levels",
            ex_id,
            step_label
        );
    }
    if c.expect_non_empty_ask_cascade == Some(true) {
        assert!(
            !cascade.levels.ask_levels.is_empty(),
            "{} {}: expected ask cascade levels",
            ex_id,
            step_label
        );
    }
    if c.expect_empty_ask_cascade == Some(true) {
        assert!(
            cascade.levels.ask_levels.is_empty(),
            "{} {}: expected no ask cascade levels",
            ex_id,
            step_label
        );
    }
}

#[test]
fn arb_test_examples_json_scenarios() {
    let f: Fixture = serde_json::from_str(FIXTURE).expect("parse arb_test_examples.json");
    for ex in &f.examples {
        for step in &ex.steps {
            let mut kb = step.kalshi.bids.clone();
            let mut ka = step.kalshi.asks.clone();
            sort_kalshi(&mut kb, &mut ka);
            assert!(
                !kb.is_empty() && !ka.is_empty(),
                "{} {}: kalshi book empty",
                ex.id,
                step.label
            );

            let mut pb = step.poly.bids.clone();
            let mut pa = step.poly.asks.clone();
            sort_poly(&mut pb, &mut pa);
            assert!(
                !pb.is_empty() && !pa.is_empty(),
                "{} {}: poly book empty",
                ex.id,
                step.label
            );

            let poly = PolyFullBookPayload { bids: pb, asks: pa };
            let (k_bid, k_ask, p_bid, p_ask) = touches(&kb, &ka, &poly);

            let mut st = MakerState::init();
            st.kalshi_balance = f.defaults.kalshi_balance;
            st.poly_balance = f.defaults.poly_balance;
            st.side_cap = f.defaults.side_cap;
            st.yes_bids = kb;
            st.yes_asks = ka;

            let cascade = build_cascade(&st, &poly);
            assert_checks(
                &ex.id,
                &step.label,
                k_bid,
                k_ask,
                p_bid,
                p_ask,
                &step.checks,
                &cascade,
            );
        }
    }
}
