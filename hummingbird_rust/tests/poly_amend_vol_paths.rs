//! Exercise `apply_poly_vol_to_one_slot` volume→qty mapping with a logging `MakerAmendOps` mock.

use hummingbird_rust::maker_runtime::{
    apply_poly_vol_to_one_slot, MakerAmendOps, MakerOrderSlot, MakerResizeOutcome,
};
use std::sync::Mutex;

struct LogOps(Mutex<Vec<String>>);

impl MakerAmendOps for LogOps {
    fn log_tag(&self) -> &'static str {
        "test"
    }
    fn cancel_one(&self, _: &str) -> bool {
        true
    }
    fn cancel_slot(&self, slot: &mut MakerOrderSlot) -> bool {
        self.0.lock().unwrap().push("cancel_slot".into());
        slot.mark_dead();
        true
    }
    fn resize_slot(
        &self,
        slot: &mut MakerOrderSlot,
        _action: &str,
        new_count: i32,
    ) -> MakerResizeOutcome {
        let old = slot.current_count;
        if new_count > old {
            self.0
                .lock()
                .unwrap()
                .push(format!("up_delta:{}", new_count - old));
        } else if new_count < old {
            self.0
                .lock()
                .unwrap()
                .push(format!("down:{}->{}", old, new_count));
        }
        slot.current_count = new_count;
        MakerResizeOutcome::Ok
    }
}

struct FailingOps;

impl MakerAmendOps for FailingOps {
    fn log_tag(&self) -> &'static str {
        "test-fail"
    }
    fn cancel_one(&self, _: &str) -> bool {
        false
    }
    fn cancel_slot(&self, _: &mut MakerOrderSlot) -> bool {
        false
    }
    fn resize_slot(&self, _: &mut MakerOrderSlot, _: &str, _: i32) -> MakerResizeOutcome {
        MakerResizeOutcome::FailedNoChange
    }
}

/// Simulates `PolyMakerOps` downsize: replace failed, prior size restored (`LiveAfterDownsizeRetry`).
struct DownsizeRetryOps;

impl MakerAmendOps for DownsizeRetryOps {
    fn log_tag(&self) -> &'static str {
        "test-downsize-retry"
    }
    fn cancel_one(&self, _: &str) -> bool {
        true
    }
    fn cancel_slot(&self, _: &mut MakerOrderSlot) -> bool {
        true
    }
    fn resize_slot(&self, _: &mut MakerOrderSlot, _: &str, _: i32) -> MakerResizeOutcome {
        MakerResizeOutcome::LiveAfterDownsizeRetry
    }
}

fn base_slot() -> MakerOrderSlot {
    MakerOrderSlot {
        order_id: "primary".into(),
        level_price_cents: 50,
        yes_price_cents: 51,
        original_count: 20,
        current_count: 10,
        initial_taker_vol: 100.0,
        initial_maker_vol: 500.0,
        child_orders: vec![],
        last_matched_reported: 0.0,
    }
}

#[test]
fn vol_increase_maps_to_resize_not_cancel() {
    let ops = LogOps(Mutex::new(vec![]));
    let mut slot = base_slot();
    apply_poly_vol_to_one_slot(&ops, &mut slot, 70.0, "buy");
    // ratio 70/100 * original 20 = 14 contracts; increase 10 -> 14 ⇒ delta 4
    let g = ops.0.lock().unwrap();
    assert!(
        !g.iter().any(|s| s == "cancel_slot"),
        "increase must not cancel_slot: {g:?}"
    );
    assert!(
        g.iter().any(|s| s == "up_delta:4"),
        "expected delta 4 place, got {g:?}"
    );
    assert_eq!(slot.current_count, 14);
    assert_eq!(slot.original_count, 14);
}

#[test]
fn vol_decrease_maps_to_downsize_path() {
    let ops = LogOps(Mutex::new(vec![]));
    let mut slot = base_slot();
    apply_poly_vol_to_one_slot(&ops, &mut slot, 30.0, "buy");
    // 30/100 * 20 = 6; decrease 10 -> 6
    let g = ops.0.lock().unwrap();
    assert!(
        g.iter().any(|s| *s == "down:10->6"),
        "expected decrease resize, got {g:?}"
    );
    assert_eq!(slot.current_count, 6);
    assert_eq!(slot.original_count, 6);
}

#[test]
fn vol_to_zero_cancels_slot() {
    let ops = LogOps(Mutex::new(vec![]));
    let mut slot = base_slot();
    apply_poly_vol_to_one_slot(&ops, &mut slot, 0.0, "buy");
    let g = ops.0.lock().unwrap();
    assert!(g.contains(&"cancel_slot".into()), "expected cancel: {g:?}");
    assert_eq!(slot.original_count, 0);
}

#[test]
fn live_after_downsize_retry_reanchors_vol_and_original() {
    let ops = DownsizeRetryOps;
    let mut slot = base_slot();
    let result = apply_poly_vol_to_one_slot(&ops, &mut slot, 70.0, "buy");

    assert_eq!(result, None);
    assert!((slot.initial_taker_vol - 70.0).abs() < 1e-9);
    assert_eq!(slot.original_count, 10);
}

#[test]
fn failed_resize_does_not_reanchor_when_venue_unchanged() {
    let ops = FailingOps;
    let mut slot = base_slot();
    let old_count = slot.current_count;
    let old_vol = slot.initial_taker_vol;
    let result = apply_poly_vol_to_one_slot(&ops, &mut slot, 70.0, "buy");

    assert_eq!(result, None);
    assert_eq!(slot.current_count, old_count);
    assert!((slot.initial_taker_vol - old_vol).abs() < 1e-9);
    assert_eq!(slot.original_count, 20);
}

#[test]
fn failed_cancel_does_not_rebase_volume_or_report_success() {
    let ops = FailingOps;
    let mut slot = base_slot();
    let old_count = slot.current_count;
    let old_vol = slot.initial_taker_vol;
    let result = apply_poly_vol_to_one_slot(&ops, &mut slot, 0.0, "buy");

    assert_eq!(result, None);
    assert_eq!(slot.current_count, old_count);
    assert!((slot.initial_taker_vol - old_vol).abs() < 1e-9);
    assert_eq!(slot.original_count, 20);
}
