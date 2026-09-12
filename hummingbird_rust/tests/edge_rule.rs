//! Integration: 2¢ edge vs taker BBO (strategy gating).

use hummingbird_rust::strategy::{build_cascade, MakerState};
use hummingbird_rust::types::{PolyFullBookPayload, PriceLevel};

fn st() -> MakerState {
    let mut m = MakerState::init();
    m.kalshi_balance = 10_000.0;
    m.poly_balance = 10_000.0;
    m.side_cap = 10_000.0;
    m.yes_bids = vec![PriceLevel {
        price: 50.0,
        size: 100.0,
    }];
    m.yes_asks = vec![PriceLevel {
        price: 55.0,
        size: 100.0,
    }];
    m
}

#[test]
fn bid_skipped_when_limit_plus_edge_not_le_taker_bid() {
    let m = st();
    let book = PolyFullBookPayload {
        bids: vec![PriceLevel {
            price: 0.52,
            size: 50.0,
        }],
        asks: vec![PriceLevel {
            price: 0.60,
            size: 50.0,
        }],
    };
    let c = build_cascade(&m, &book);
    assert!(
        c.levels.bid_levels.is_empty(),
        "bid cascade should skip when (k_bid+1)+EDGE > p_bid"
    );
}

#[test]
fn ask_skipped_when_k_ask_not_gt_taker_ask_plus_edge() {
    let m = st();
    let book = PolyFullBookPayload {
        bids: vec![PriceLevel {
            price: 0.40,
            size: 50.0,
        }],
        asks: vec![PriceLevel {
            price: 0.53,
            size: 50.0,
        }],
    };
    let c = build_cascade(&m, &book);
    assert!(
        c.levels.ask_levels.is_empty(),
        "ask cascade should skip when k_ask <= p_ask + EDGE"
    );
}
