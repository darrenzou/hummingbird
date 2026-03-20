use crate::arb_config::{now_ms, ArbCreds};
use crate::arb_db::ArbDb;
use crate::arb_ipc;
use crate::error_policy;
use crate::kalshi_live::{KalshiBatchOrder, KalshiLive, BATCH_MAX};
use crate::types::*;
use anyhow::Context;
use std::env;
use std::os::unix::io::RawFd;
use std::time::Duration;

const MAX_RETRIES: usize = 8;

pub use crate::strategy::{build_cascade, cascade_result_to_orders, CascadeResult, KalshiState};

struct OrderSlot {
    order_id: String,
    /// Level key matching Poly tracked price (k_bid / k_ask).
    level_price_cents: i16,
    /// Kalshi REST `yes_price` for this order.
    yes_price_cents: i16,
    original_count: i32,
    current_count: i32,
    initial_poly_vol: f64,
}

fn send_abort_fatal(fd_out: RawFd, code: &str, message: &str) {
    let _ = arb_ipc::send_msg(
        fd_out,
        &ArbMsg::AbortFatal(AbortFatalPayload {
            version: IPC_VERSION,
            ts: now_ms(),
            reason_code: code.to_string(),
            message: message.to_string(),
        }),
    );
}

fn batch_place_with_retry(
    live: &KalshiLive,
    orders: &[KalshiBatchOrder],
    db: &Option<ArbDb>,
    market: &str,
    kb: &[PriceLevel],
    ka: &[PriceLevel],
    pb: &[PriceLevel],
    pa: &[PriceLevel],
) -> anyhow::Result<Vec<String>> {
    let mut attempt = 0usize;
    loop {
        let (ids, status) = live.batch_place_orders(orders)?;
        if error_policy::http_is_rate_limited(status) {
            if let Some(db) = db {
                let _ = db.record_error(
                    market,
                    "kalshi",
                    "rate_limit",
                    &format!("batch_place HTTP {status}"),
                    Some(status),
                    true,
                    kb,
                    ka,
                    pb,
                    pa,
                    &serde_json::json!({}),
                );
            }
            if attempt < MAX_RETRIES {
                let backoff = 500u64 * (1u64 << attempt.min(6));
                eprintln!(
                    "[kalshi] rate limited, retry {}/{} in {}ms",
                    attempt + 1,
                    MAX_RETRIES,
                    backoff
                );
                std::thread::sleep(Duration::from_millis(backoff));
                attempt += 1;
                continue;
            }
            anyhow::bail!("batch place: rate limited after {MAX_RETRIES} retries");
        }
        if status == 0 {
            if attempt < MAX_RETRIES {
                std::thread::sleep(Duration::from_millis(300 * (attempt as u64 + 1)));
                attempt += 1;
                continue;
            }
            anyhow::bail!("batch place: network error");
        }
        if status >= 400 {
            anyhow::bail!("batch place: HTTP {status}");
        }
        return Ok(ids);
    }
}

fn find_level_price(slots: &[OrderSlot], oid: &str) -> (i16, i16) {
    for s in slots {
        if s.order_id == oid {
            return (s.level_price_cents, s.yes_price_cents);
        }
    }
    (0, 0)
}

fn merge_order_ids(orders: &[CascadeOrder], bid_ids: &[String], ask_ids: &[String]) -> Vec<String> {
    let mut bi = 0usize;
    let mut ai = 0usize;
    let mut out = Vec::with_capacity(orders.len());
    for o in orders {
        if o.side == Side::Bid {
            if bi < bid_ids.len() {
                out.push(bid_ids[bi].clone());
                bi += 1;
            } else {
                out.push(String::new());
            }
        } else if ai < ask_ids.len() {
            out.push(ask_ids[ai].clone());
            ai += 1;
        } else {
            out.push(String::new());
        }
    }
    out
}

fn orders_to_slots(orders: &[CascadeOrder], ids: &[String]) -> anyhow::Result<(Vec<OrderSlot>, Vec<OrderSlot>)> {
    if ids.len() != orders.len() {
        anyhow::bail!(
            "kalshi order id mismatch: got {} ids for {} orders",
            ids.len(),
            orders.len()
        );
    }
    let mut bid_slots = Vec::new();
    let mut ask_slots = Vec::new();
    for (i, co) in orders.iter().enumerate() {
        let oid = ids[i].clone();
        if oid.is_empty() {
            anyhow::bail!("empty kalshi order id at index {i}");
        }
        let slot = OrderSlot {
            order_id: oid,
            level_price_cents: co.level_price_cents,
            yes_price_cents: co.limit_price_cents,
            original_count: co.qty as i32,
            current_count: co.qty as i32,
            initial_poly_vol: co.initial_poly_vol,
        };
        if co.side == Side::Bid {
            bid_slots.push(slot);
        } else {
            ask_slots.push(slot);
        }
    }
    Ok((bid_slots, ask_slots))
}

pub fn kalshi_process_run(fd_in: RawFd, fd_out: RawFd) -> anyhow::Result<()> {
    let creds = ArbCreds::from_env();
    let ticker = env::var("ARB_TICKER").context("ARB_TICKER not set")?;
    let market = ticker.clone();

    eprintln!("[kalshi] connecting WS for ticker={ticker}");
    let mut live = KalshiLive::new(&creds, &ticker)?;
    live.ws_connect()?;

    let (bids_raw, asks_raw) = live.ws_copy_orderbook();

    let mut st = KalshiState::init();
    st.kalshi_balance = env::var("ARB_KALSHI_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| live.get_balance());
    st.poly_balance = env::var("ARB_POLY_BALANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000.0);
    st.side_cap = env::var("ARB_SIDE_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000.0);

    st.yes_bids = bids_raw
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    st.yes_asks = asks_raw
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    st.yes_bids
        .sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    st.yes_asks
        .sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

    eprintln!(
        "[kalshi] orderbook: {} bids, {} asks | balance={:.2}",
        st.yes_bids.len(),
        st.yes_asks.len(),
        st.kalshi_balance
    );

    let snap = KalshiBookSnapshot {
        version: IPC_VERSION,
        market: market.clone(),
        ts: now_ms(),
        bids: st.yes_bids.clone(),
        asks: st.yes_asks.clone(),
    };
    arb_ipc::send_msg(fd_out, &ArbMsg::KalshiBookSnapshot(snap)).context("send kalshi snapshot")?;

    let msg: ArbMsg = arb_ipc::recv_msg(fd_in).context("kalshi recv cascade")?;
    let cascade_msg = match msg {
        ArbMsg::CascadeOrders(c) => c,
        ArbMsg::AbortFatal(a) => {
            eprintln!("[kalshi] poly abort_fatal: {}", a.message);
            return Ok(());
        }
        ArbMsg::Abort { reason } => {
            eprintln!("[kalshi] poly abort: {reason}");
            return Ok(());
        }
        other => anyhow::bail!("[kalshi] expected CascadeOrders, got {other:?}"),
    };

    let db = ArbDb::open(&creds).ok();

    let kb = st.yes_bids.clone();
    let ka = st.yes_asks.clone();
    let poly_empty = PolyFullBookPayload::default();
    if let Some(db) = &db {
        let _ = db.record_start(&market, &kb, &ka, &poly_empty.bids, &poly_empty.asks);
    }

    let orders = cascade_msg.orders;
    if orders.is_empty() {
        eprintln!("[kalshi] empty cascade orders");
        send_abort_fatal(fd_out, "no_orders", "empty CascadeOrders");
        return Ok(());
    }

    let bid_batch: Vec<KalshiBatchOrder> = orders
        .iter()
        .filter(|o| o.side == Side::Bid)
        .map(|o| KalshiBatchOrder {
            action: "buy".to_string(),
            count: o.qty as i32,
            yes_price: o.limit_price_cents as i32,
            client_order_id: None,
        })
        .collect();
    let ask_batch: Vec<KalshiBatchOrder> = orders
        .iter()
        .filter(|o| o.side == Side::Ask)
        .map(|o| KalshiBatchOrder {
            action: "sell".to_string(),
            count: o.qty as i32,
            yes_price: o.limit_price_cents as i32,
            client_order_id: None,
        })
        .collect();

    let (k_bids, k_asks) = live.ws_copy_orderbook();
    let kb_now: Vec<PriceLevel> = k_bids
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();
    let ka_now: Vec<PriceLevel> = k_asks
        .iter()
        .map(|&(p, s)| PriceLevel { price: p, size: s })
        .collect();

    let mut bid_ids: Vec<String> = Vec::new();
    if !bid_batch.is_empty() {
        for chunk in bid_batch.chunks(BATCH_MAX) {
            match batch_place_with_retry(
                &live,
                chunk,
                &db,
                &market,
                &kb_now,
                &ka_now,
                &poly_empty.bids,
                &poly_empty.asks,
            ) {
                Ok(ids) => bid_ids.extend(ids),
                Err(e) => {
                    eprintln!("[kalshi] bid batch failed: {e}");
                    let _ = db.as_ref().map(|d| {
                        d.record_error(
                            &market,
                            "kalshi",
                            "place_failed",
                            &e.to_string(),
                            None,
                            false,
                            &kb_now,
                            &ka_now,
                            &poly_empty.bids,
                            &poly_empty.asks,
                            &serde_json::json!({}),
                        )
                    });
                    send_abort_fatal(fd_out, "bid_place_failed", &e.to_string());
                    return Ok(());
                }
            }
        }
    }

    let mut ask_ids: Vec<String> = Vec::new();
    if !ask_batch.is_empty() {
        for chunk in ask_batch.chunks(BATCH_MAX) {
            match batch_place_with_retry(
                &live,
                chunk,
                &db,
                &market,
                &kb_now,
                &ka_now,
                &poly_empty.bids,
                &poly_empty.asks,
            ) {
                Ok(ids) => ask_ids.extend(ids),
                Err(e) => {
                    eprintln!("[kalshi] ask batch failed: {e}");
                    for oid in &bid_ids {
                        let _ = live.cancel_order(oid);
                    }
                    let _ = db.as_ref().map(|d| {
                        d.record_error(
                            &market,
                            "kalshi",
                            "place_failed",
                            &e.to_string(),
                            None,
                            false,
                            &kb_now,
                            &ka_now,
                            &poly_empty.bids,
                            &poly_empty.asks,
                            &serde_json::json!({}),
                        )
                    });
                    send_abort_fatal(fd_out, "ask_place_failed", &e.to_string());
                    return Ok(());
                }
            }
        }
    }

    let merged_ids = merge_order_ids(&orders, &bid_ids, &ask_ids);
    let (mut bid_slots, mut ask_slots) = orders_to_slots(&orders, &merged_ids)?;

    let levels_done = KalshiLevelsDonePayload {
        bid_levels: bid_slots
            .iter()
            .map(|s| (s.level_price_cents, s.current_count as f64))
            .collect(),
        ask_levels: ask_slots
            .iter()
            .map(|s| (s.level_price_cents, s.current_count as f64))
            .collect(),
    };

    arb_ipc::send_msg(fd_out, &ArbMsg::KalshiLevelsDone(levels_done))
        .context("kalshi send levels")?;

    live.ws_subscribe_fills();

    let mut position: i64 = 0;
    let max_iterations = 12000;
    let mut abort_reason: Option<String> = None;

    for _ in 0..max_iterations {
        if live.ws_done() {
            abort_reason = Some("kalshi_ws_disconnected".to_string());
            break;
        }

        if arb_ipc::poll_readable(fd_in, 50).unwrap_or(false) {
            let m: ArbMsg = match arb_ipc::recv_msg(fd_in) {
                Ok(m) => m,
                Err(_) => break,
            };
            match m {
                ArbMsg::Abort { reason } | ArbMsg::KalshiAbort { reason } => {
                    abort_reason = Some(reason);
                    break;
                }
                ArbMsg::AbortFatal(a) => {
                    abort_reason = Some(format!("{}:{}", a.reason_code, a.message));
                    break;
                }
                ArbMsg::PolyLevelVolUpdate(u) => {
                    handle_volume_update(&live, &u, &mut bid_slots, &mut ask_slots, &db, &market);
                }
                ArbMsg::LevelUpdate(u) => {
                    handle_level_update_ipc(&live, &u, &mut bid_slots, &mut ask_slots, &db, &market);
                }
                _ => {}
            }
        }

        live.ws_service(50);

        while let Some((count, is_bid, oid)) = live.ws_poll_fill() {
            let slots_ref = if is_bid {
                &bid_slots
            } else {
                &ask_slots
            };
            let (level_pc, yes_pc) = find_level_price(slots_ref, &oid);
            let price_cents = if yes_pc > 0 {
                yes_pc
            } else {
                level_pc
            };

            let slots = if is_bid {
                &mut bid_slots
            } else {
                &mut ask_slots
            };
            if let Some(slot) = slots.iter_mut().find(|s| s.order_id == oid) {
                slot.current_count = (slot.current_count - count as i32).max(0);
            }

            if is_bid {
                position += count as i64;
            } else {
                position -= count as i64;
            }

            eprintln!(
                "[kalshi] fill: {} x{} at {}c oid={}",
                if is_bid { "BUY" } else { "SELL" },
                count,
                price_cents,
                &oid[..8.min(oid.len())]
            );

            let (k_bids, k_asks) = live.ws_copy_orderbook();
            let kb: Vec<PriceLevel> = k_bids
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            let ka: Vec<PriceLevel> = k_asks
                .iter()
                .map(|&(p, s)| PriceLevel { price: p, size: s })
                .collect();
            if let Some(db) = &db {
                let _ = db.record_fill(
                    &market,
                    &kb,
                    &ka,
                    &[],
                    &[],
                    price_cents as i32,
                    count,
                );
            }

            let fill = KalshiFillPayload {
                ts: now_ms(),
                order_id: oid.clone(),
                side: if is_bid { Side::Bid } else { Side::Ask },
                price_cents,
                filled_count: count,
                market: market.clone(),
            };
            let _ = arb_ipc::send_msg(fd_out, &ArbMsg::KalshiFill(fill));
        }
    }

    for slot in bid_slots.iter().chain(ask_slots.iter()) {
        if !slot.order_id.is_empty() && slot.current_count > 0 {
            let _ = live.cancel_order(&slot.order_id);
        }
    }

    if let Some(reason) = &abort_reason {
        let (k_bids, k_asks) = live.ws_copy_orderbook();
        let kb: Vec<PriceLevel> = k_bids
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        let ka: Vec<PriceLevel> = k_asks
            .iter()
            .map(|&(p, s)| PriceLevel { price: p, size: s })
            .collect();
        if let Some(db) = &db {
            let _ = db.record_abort(&market, reason, &kb, &ka, &[], &[]);
        }
        let _ = arb_ipc::send_msg(
            fd_out,
            &ArbMsg::KalshiAbort {
                reason: reason.clone(),
            },
        );
    }

    eprintln!("[kalshi] done, final position: {position}");
    Ok(())
}

fn handle_volume_update(
    live: &KalshiLive,
    update: &PolyLevelVolUpdatePayload,
    bid_slots: &mut [OrderSlot],
    ask_slots: &mut [OrderSlot],
    db: &Option<ArbDb>,
    market: &str,
) {
    let slots = if update.side == Side::Bid {
        bid_slots
    } else {
        ask_slots
    };
    let action = if update.side == Side::Bid {
        "buy"
    } else {
        "sell"
    };

    for (price_cents, new_vol) in &update.levels {
        if let Some(slot) = slots
            .iter_mut()
            .find(|s| s.level_price_cents == *price_cents)
        {
            let orig = slot.original_count as f64;
            if orig <= 0.0 {
                continue;
            }

            let new_count = if *new_vol <= 0.0 || slot.initial_poly_vol <= 0.0 {
                0
            } else {
                let ratio = *new_vol / slot.initial_poly_vol;
                (orig * ratio).floor().max(0.0) as i32
            };

            let old_count = slot.current_count;
            if new_count <= 0 && slot.current_count > 0 {
                if live.cancel_order(&slot.order_id) {
                    eprintln!(
                        "[kalshi] cancelled {} at {}c (vol→0)",
                        action, slot.yes_price_cents
                    );
                    slot.current_count = 0;
                }
            } else if new_count != slot.current_count && new_count > 0 {
                if live.amend_order(
                    &slot.order_id,
                    "yes",
                    action,
                    slot.yes_price_cents as i32,
                    new_count,
                ) {
                    eprintln!(
                        "[kalshi] amended {} at {}c: {} → {}",
                        action, slot.yes_price_cents, slot.current_count, new_count
                    );
                    slot.current_count = new_count;
                }
            }

            if let Some(db) = db {
                let _ = db.record_resize(
                    &[],
                    &[],
                    &[],
                    &[],
                    *price_cents as i32,
                    update.side == Side::Bid,
                    old_count as f64,
                    *new_vol,
                );
            }
        }
    }
    let _ = market;
}

fn handle_level_update_ipc(
    live: &KalshiLive,
    u: &LevelUpdate,
    bid_slots: &mut [OrderSlot],
    ask_slots: &mut [OrderSlot],
    db: &Option<ArbDb>,
    market: &str,
) {
    for item in &u.updates {
        let slots_bid = bid_slots
            .iter_mut()
            .find(|s| s.level_price_cents == item.level_price_cents);
        let slots_ask = ask_slots
            .iter_mut()
            .find(|s| s.level_price_cents == item.level_price_cents);
        let (slot, action) = if let Some(s) = slots_bid {
            (s, "buy")
        } else if let Some(s) = slots_ask {
            (s, "sell")
        } else {
            continue;
        };

        match item.action {
            LevelAction::Cancel => {
                if slot.current_count > 0 && live.cancel_order(&slot.order_id) {
                    eprintln!(
                        "[kalshi] LevelUpdate cancel at level {}c",
                        item.level_price_cents
                    );
                    slot.current_count = 0;
                }
            }
            LevelAction::Amend => {
                if let Some(nq) = item.new_qty {
                    let nc = nq as i32;
                    if nc <= 0 {
                        if live.cancel_order(&slot.order_id) {
                            slot.current_count = 0;
                        }
                    } else if nc != slot.current_count {
                        if live.amend_order(
                            &slot.order_id,
                            "yes",
                            action,
                            slot.yes_price_cents as i32,
                            nc,
                        ) {
                            slot.current_count = nc;
                        }
                    }
                }
            }
        }
        let _ = (db, market);
    }
}
