//! Taker-side hedge accumulator and shared taker helpers.

/// Fire a single hedge when `|pending_signed|` exceeds this many **contracts**.
pub const HEDGE_ACCUM_THRESHOLD: i32 = 10;

/// If `|acc| > threshold`, reset `acc` to 0 and return `Some(signed_qty)` to hedge; else `None`.
#[inline]
pub fn hedge_accum_take_if_over_throttle(acc: &mut i32, threshold: i32) -> Option<i32> {
    if acc.unsigned_abs() <= threshold as u32 {
        return None;
    }
    let q = *acc;
    *acc = 0;
    Some(q)
}

/// Venue-specific operations for a future shared taker outer loop (Poly-as-taker vs Kalshi-as-taker).
///
/// Today the live loops live in [`crate::arb_poly::poly_taker_process_run`] and
/// [`crate::kalshi_taker::kalshi_taker_process_run`]; they differ mainly around the Polymarket
/// pre-sign pool and idle-merge hooks.
#[allow(dead_code)]
pub trait TakerVenueOps {
    fn market(&self) -> &str;
    fn ws_service(&mut self, ms: u64);
    fn ws_done(&self) -> bool;
}
