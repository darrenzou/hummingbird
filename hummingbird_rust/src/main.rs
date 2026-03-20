use anyhow::Context;
use std::env;
use std::os::unix::io::RawFd;

fn main() -> anyhow::Result<()> {
    let config_path = env::args().nth(1).unwrap_or_else(|| "config.json".to_string());

    // Load .env first (like hummingbirdv2), then config.json.
    hummingbird_rust::arb_config::load_dotenv(".env");
    let creds = hummingbird_rust::arb_config::ArbCreds::from_env();

    let cfg = hummingbird_rust::arb_config::load_config(&config_path)
        .with_context(|| format!("failed to load config from {config_path}"))?;

    // Mirror v2: require creds (no sim mode by default).
    if creds.kalshi_api_key_id.is_empty() || creds.kalshi_private_key_pem.is_empty() {
        anyhow::bail!(
            "[main] Kalshi creds required (KALSHI_API_KEY_ID, KALSHI_PRIVATE_KEY_PATH or KALSHI_PRIVATE_KEY_PEM)"
        );
    }
    if creds.poly_address.is_empty() || creds.eth_priv_key.is_empty() {
        anyhow::bail!("[main] Poly creds required (POLY_ADDRESS, ETH_PRIV_KEY, etc.)");
    }

    // Apply pair 0 (like v2).
    let pair0 = cfg
        .pairs
        .get(0)
        .context("config must include at least one pair")?;
    env::set_var("ARB_LIVE", "1");
    env::set_var("ARB_TICKER", &pair0.kalshi_ticker);
    env::set_var("ARB_TOKEN_ID", &pair0.polymarket_token_id);
    if let Some(no_token) = &pair0.polymarket_no_token_id {
        if !no_token.is_empty() {
            env::set_var("ARB_NO_TOKEN_ID", no_token);
        }
    }
    env::set_var("ARB_NEG_RISK", if pair0.neg_risk { "1" } else { "0" });

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

    eprintln!(
        "[main] ticker={} token={}",
        pair0.kalshi_ticker, pair0.polymarket_token_id
    );

    // Two pipes: poly -> kalshi, kalshi -> poly.
    let (poly_to_kalshi_r, poly_to_kalshi_w) = hummingbird_rust::arb_ipc::pipe_pair()?;
    let (kalshi_to_poly_r, kalshi_to_poly_w) = hummingbird_rust::arb_ipc::pipe_pair()?;

    unsafe {
        let poly_pid = libc::fork();
        if poly_pid < 0 {
            anyhow::bail!("fork poly failed");
        }
        if poly_pid == 0 {
            // Poly child
            hummingbird_rust::arb_ipc::close_fd(poly_to_kalshi_r);
            hummingbird_rust::arb_ipc::close_fd(kalshi_to_poly_w);
            let fd_out: RawFd = poly_to_kalshi_w;
            let fd_in: RawFd = kalshi_to_poly_r;
            let _ = hummingbird_rust::arb_poly::poly_process_run(fd_in, fd_out);
            hummingbird_rust::arb_ipc::close_fd(fd_out);
            hummingbird_rust::arb_ipc::close_fd(fd_in);
            libc::_exit(0);
        }

        let kalshi_pid = libc::fork();
        if kalshi_pid < 0 {
            anyhow::bail!("fork kalshi failed");
        }
        if kalshi_pid == 0 {
            // Kalshi child
            hummingbird_rust::arb_ipc::close_fd(poly_to_kalshi_w);
            hummingbird_rust::arb_ipc::close_fd(kalshi_to_poly_r);
            let fd_in: RawFd = poly_to_kalshi_r;
            let fd_out: RawFd = kalshi_to_poly_w;
            let _ = hummingbird_rust::arb_kalshi::kalshi_process_run(fd_in, fd_out);
            hummingbird_rust::arb_ipc::close_fd(fd_out);
            hummingbird_rust::arb_ipc::close_fd(fd_in);
            libc::_exit(0);
        }

        // Parent: close all fds and wait.
        hummingbird_rust::arb_ipc::close_fd(poly_to_kalshi_r);
        hummingbird_rust::arb_ipc::close_fd(poly_to_kalshi_w);
        hummingbird_rust::arb_ipc::close_fd(kalshi_to_poly_r);
        hummingbird_rust::arb_ipc::close_fd(kalshi_to_poly_w);

        let mut status: libc::c_int = 0;
        libc::waitpid(poly_pid, &mut status, 0);
        libc::waitpid(kalshi_pid, &mut status, 0);
        eprintln!("[parent] both child processes exited");
    }

    Ok(())
}

