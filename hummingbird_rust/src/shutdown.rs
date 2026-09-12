//! Ctrl+C / cooperative shutdown for child processes (Poly + Kalshi).

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Register a handler so Ctrl+C sets [`shutdown_requested`] instead of killing the process abruptly.
pub fn install_shutdown_handler() -> anyhow::Result<()> {
    ctrlc::set_handler(|| {
        eprintln!(
            "\n[shutdown] interrupt — finishing cleanly (cancel Kalshi orders, notify peer)…"
        );
        SHUTDOWN.store(true, Ordering::SeqCst);
    })
    .map_err(|e| anyhow::anyhow!("ctrlc: {e}"))
}

#[inline]
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}
