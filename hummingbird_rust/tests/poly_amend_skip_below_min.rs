//! Polymarket open-size increase requires `delta >= order_min_size` (`PolyMakerOps::resize_slot`).

use hummingbird_rust::arb_poly::poly_increase_delta_meets_min_size;

#[test]
fn increase_delta_below_min_is_rejected() {
    assert!(!poly_increase_delta_meets_min_size(3, 5));
    assert!(!poly_increase_delta_meets_min_size(4, 5));
}

#[test]
fn increase_delta_at_or_above_min_is_allowed() {
    assert!(poly_increase_delta_meets_min_size(5, 5));
    assert!(poly_increase_delta_meets_min_size(10, 5));
}

#[test]
fn min_size_below_one_is_treated_as_one() {
    assert!(!poly_increase_delta_meets_min_size(0, 0)); // delta 0 < 1
}
