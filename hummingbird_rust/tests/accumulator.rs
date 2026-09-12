//! Signed hedge accumulator on the taker.

use hummingbird_rust::strategy::hedge_accum_apply;
use hummingbird_rust::taker_runtime::{hedge_accum_take_if_over_throttle, HEDGE_ACCUM_THRESHOLD};
use hummingbird_rust::types::Side;

#[test]
fn fires_only_over_threshold_full_buffer() {
    let mut acc = 0i32;
    hedge_accum_apply(&mut acc, Side::Ask, 5);
    assert_eq!(
        hedge_accum_take_if_over_throttle(&mut acc, HEDGE_ACCUM_THRESHOLD),
        None
    );
    hedge_accum_apply(&mut acc, Side::Ask, 7);
    let q = hedge_accum_take_if_over_throttle(&mut acc, HEDGE_ACCUM_THRESHOLD);
    assert_eq!(q, Some(12));
    assert_eq!(acc, 0);
}

#[test]
fn does_not_fire_at_or_below_threshold() {
    let mut acc = 0i32;
    hedge_accum_apply(&mut acc, Side::Ask, 5);
    assert_eq!(
        hedge_accum_take_if_over_throttle(&mut acc, HEDGE_ACCUM_THRESHOLD),
        None
    );
    assert_eq!(acc, 5);
}
