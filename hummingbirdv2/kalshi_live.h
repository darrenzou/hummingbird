/*
 * Kalshi live API: REST (place/cancel/amend/balance) + WebSocket (orderbook, user_fills).
 * Uses credentials from ArbCreds; no order placement via WS (Kalshi uses REST for that).
 */
#ifndef KALSHI_LIVE_H
#define KALSHI_LIVE_H

#include <stdint.h>
#include "arb_config.h"

typedef struct KalshiLive KalshiLive;

/* Create/destroy. Ticker = Kalshi market ticker (e.g. KXLLM1-26MAR31-A). */
KalshiLive *kalshi_live_create(const ArbCreds *creds, const char *ticker);
void kalshi_live_destroy(KalshiLive *k);

#define KALSHI_BATCH_MAX 20  /* Kalshi API cap per batch request */

/* One order in a batch (yes_price in cents). action is "buy" or "sell"; side is "yes". */
typedef struct {
    const char *action;   /* "buy" or "sell" */
    int count;
    int yes_price;
    char client_order_id[64];  /* optional; can be empty */
} KalshiBatchOrder;

/*
 * Batch place up to 20 orders (Kalshi limit). orders[0..n_orders-1], order_id_out[0..n_orders-1] (each 64 chars).
 * Returns number of orders that succeeded (order_id_out[i] set); or -1 on HTTP/request failure.
 * On failure, http_status_out (if non-NULL) is set to the HTTP code (0 = no response/network).
 * Per-order failure: order_id_out[i] left empty. See KALSHI_BATCH_ERRORS in kalshi_live.c for possible errors.
 */
int kalshi_live_batch_place_orders(KalshiLive *k, const KalshiBatchOrder *orders, int n_orders, char (*order_id_out)[64], long *http_status_out);

/* REST: place single limit order. yes_price in cents. Returns 1 on success, 0 on failure. order_id_out at least 64 chars. */
int kalshi_live_place_order(KalshiLive *k, const char *side, const char *action, int count, int yes_price, char *order_id_out);
int kalshi_live_cancel_order(KalshiLive *k, const char *order_id);  /* 1 = success, 0 = failure (retryable) */
int kalshi_live_amend_order(KalshiLive *k, const char *order_id, const char *side, const char *action, int yes_price, int new_count);
double kalshi_live_get_balance(KalshiLive *k);

/* WebSocket: connect (blocking until connected or fail), then call service in a loop. */
int kalshi_live_ws_connect(KalshiLive *k);  /* 0 = fail */
void kalshi_live_ws_service(KalshiLive *k, int timeout_ms);
int kalshi_live_ws_got_orderbook(KalshiLive *k);
void kalshi_live_ws_copy_orderbook(KalshiLive *k, double *yes_bids, double *bid_sizes, int *n_bids, double *yes_asks, double *ask_sizes, int *n_asks, int max_levels);
int kalshi_live_ws_done(KalshiLive *k);

/* After placing orders, subscribe to user_fills. Then poll: returns 1 if a fill was consumed. */
void kalshi_live_ws_subscribe_fills(KalshiLive *k);
int kalshi_live_ws_poll_fill(KalshiLive *k, uint32_t *fill_count_out, int *is_bid_out, char *order_id_out, int order_id_size);

#endif /* KALSHI_LIVE_H */
