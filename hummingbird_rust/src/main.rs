//! Supervisor: load config, fork venue workers, wait. Does not trade.
//!
//! After `fork` the children do not share heap. The parent ignores SIGINT so
//! each worker can cancel resting orders and notify its peer before exit.

use anyhow::Context;
use hummingbird_rust::arb_config::{self, ArbConfig, ArbCreds, MakerVenue};
use hummingbird_rust::arb_ipc;
use std::env;
use std::os::unix::io::RawFd;

fn main() -> anyhow::Result<()> {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".to_string());

    arb_config::load_dotenv(".env");
    let creds = ArbCreds::from_env();
    let cfg = arb_config::load_config(&config_path)
        .with_context(|| format!("failed to load config from {config_path}"))?;

    require_creds(&creds)?;
    let pair0 = cfg
        .pairs
        .first()
        .context("config must include at least one pair")?;
    apply_pair_to_env(pair0, &cfg);

    eprintln!(
        "[main] ticker={} token={} maker={:?}",
        pair0.kalshi_ticker, pair0.polymarket_token_id, pair0.maker
    );

    run_workers()
}

fn require_creds(creds: &ArbCreds) -> anyhow::Result<()> {
    if creds.kalshi_api_key_id.is_empty() || creds.kalshi_private_key_pem.is_empty() {
        anyhow::bail!(
            "[main] Kalshi creds required (KALSHI_API_KEY_ID, KALSHI_PRIVATE_KEY_PATH or KALSHI_PRIVATE_KEY_PEM)"
        );
    }
    if creds.poly_address.is_empty() || creds.eth_priv_key.is_empty() {
        anyhow::bail!("[main] Poly creds required (POLY_ADDRESS, ETH_PRIV_KEY, etc.)");
    }
    Ok(())
}

/// Workers read market + budget from the environment, not from a shared struct
/// (they are separate processes after `fork`).
fn apply_pair_to_env(pair: &arb_config::ArbPairConfig, cfg: &ArbConfig) {
    env::set_var("ARB_LIVE", "1");
    env::set_var("ARB_TICKER", &pair.kalshi_ticker);
    env::set_var("ARB_TOKEN_ID", &pair.polymarket_token_id);
    if let Some(no_token) = &pair.polymarket_no_token_id {
        if !no_token.is_empty() {
            env::set_var("ARB_NO_TOKEN_ID", no_token);
        }
    }
    env::set_var("ARB_NEG_RISK", if pair.neg_risk { "1" } else { "0" });
    env::set_var(
        "ARB_MAKER",
        match pair.maker {
            MakerVenue::Kalshi => "kalshi",
            MakerVenue::Polymarket => "polymarket",
        },
    );

    if let Some(db_path) = &cfg.db_path {
        if !db_path.is_empty() {
            env::set_var("ARB_DB_PATH", db_path);
        }
    }
    if let Some(side_cap) = cfg.side_cap {
        if side_cap > 0.0 {
            env::set_var("ARB_SIDE_CAP", format!("{side_cap:.0}"));
        }
    }
    if let Some(kb) = cfg.kalshi_balance {
        if kb > 0.0 {
            env::set_var("ARB_KALSHI_BALANCE", format!("{kb:.0}"));
        }
    }
    if let Some(pb) = cfg.poly_balance {
        if pb > 0.0 {
            env::set_var("ARB_POLY_BALANCE", format!("{pb:.0}"));
        }
    }
}

/// Two unidirectional pipes, then `fork` once per venue.
///
/// ```text
/// parent
///  ├── poly child    writes poly_to_kalshi, reads kalshi_to_poly
///  └── kalshi child  reads poly_to_kalshi, writes kalshi_to_poly
/// ```
fn run_workers() -> anyhow::Result<()> {
    let (poly_to_kalshi_r, poly_to_kalshi_w) = arb_ipc::pipe_pair()?;
    let (kalshi_to_poly_r, kalshi_to_poly_w) = arb_ipc::pipe_pair()?;

    unsafe {
        // Children handle SIGINT (cancel orders, notify peer). Parent stays up
        // until both have exited.
        #[cfg(unix)]
        libc::signal(libc::SIGINT, libc::SIG_IGN);

        let poly_pid = libc::fork();
        if poly_pid < 0 {
            anyhow::bail!("fork poly failed");
        }
        if poly_pid == 0 {
            arb_ipc::close_fd(poly_to_kalshi_r);
            arb_ipc::close_fd(kalshi_to_poly_w);
            let fd_out: RawFd = poly_to_kalshi_w;
            let fd_in: RawFd = kalshi_to_poly_r;
            if let Err(e) = hummingbird_rust::arb_poly::poly_process_run(fd_in, fd_out) {
                eprintln!("[poly-child] process exited with error: {e:#}");
            }
            arb_ipc::close_fd(fd_out);
            arb_ipc::close_fd(fd_in);
            libc::_exit(0);
        }

        let kalshi_pid = libc::fork();
        if kalshi_pid < 0 {
            anyhow::bail!("fork kalshi failed");
        }
        if kalshi_pid == 0 {
            arb_ipc::close_fd(poly_to_kalshi_w);
            arb_ipc::close_fd(kalshi_to_poly_r);
            let fd_in: RawFd = poly_to_kalshi_r;
            let fd_out: RawFd = kalshi_to_poly_w;
            if let Err(e) = hummingbird_rust::arb_kalshi::kalshi_process_run(fd_in, fd_out) {
                eprintln!("[kalshi-child] process exited with error: {e:#}");
            }
            arb_ipc::close_fd(fd_out);
            arb_ipc::close_fd(fd_in);
            libc::_exit(0);
        }

        arb_ipc::close_fd(poly_to_kalshi_r);
        arb_ipc::close_fd(poly_to_kalshi_w);
        arb_ipc::close_fd(kalshi_to_poly_r);
        arb_ipc::close_fd(kalshi_to_poly_w);

        let mut status: libc::c_int = 0;
        libc::waitpid(poly_pid, &mut status, 0);
        libc::waitpid(kalshi_pid, &mut status, 0);
        eprintln!("[parent] both child processes exited");
    }

    Ok(())
}
