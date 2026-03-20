/*
 * Polymarket live API: CLOB REST (place signed order, balance) + WebSocket (market orderbook).
 * Order placement is REST-only (EIP-712 signed order POST to /order).
 */
#ifndef POLY_LIVE_H
#define POLY_LIVE_H

#include <stddef.h>
#include <stdint.h>
#include "arb_config.h"
#include "arb_ipc.h"

typedef struct PolyLive PolyLive;

/* Create/destroy. token_id = Polymarket YES token (decimal string). */
PolyLive *poly_live_create(const ArbCreds *creds, const char *token_id, int neg_risk);
void poly_live_destroy(PolyLive *p);

/* REST: place one pre-signed order (body = JSON order from poly_live_build_signed_order). Returns 1 on success. */
int poly_live_place_order(PolyLive *p, const char *order_body_json);

/* Place order with error classification for retry/abort logic. Returns: */
#define POLY_PLACE_OK           0
#define POLY_PLACE_ERR_AUTH     1   /* 401/403 -> send abort */
#define POLY_PLACE_ERR_RATE     2   /* 429 -> retry after 0.5s */
#define POLY_PLACE_ERR_NETWORK  3   /* curl failed -> retry up to 5 times then abort */
#define POLY_PLACE_ERR_OTHER    4   /* other 4xx/5xx -> instant abort */
int poly_live_place_order_attempt(PolyLive *p, const char *order_body_json);
double poly_live_get_balance(PolyLive *p);

/* Build a signed GTC BUY order (uses p->token_id). */
int poly_live_build_signed_order(PolyLive *p, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size);
/* Build a signed BUY order for the given token. */
int poly_live_build_signed_buy_order(PolyLive *p, const char *token_id, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size);

/* Build a signed SELL order for the given token (for sell-first hedge). */
int poly_live_build_signed_sell_order(PolyLive *p, const char *token_id, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size);

/* Get position size (outcome tokens) for token_id via Data API. Returns 0 if not found. */
double poly_live_get_position(PolyLive *p, const char *token_id);

/* WebSocket: connect to market channel, wait for book. */
int poly_live_ws_connect(PolyLive *p);
void poly_live_ws_service(PolyLive *p, int timeout_ms);
int poly_live_ws_got_orderbook(PolyLive *p);
void poly_live_ws_copy_orderbook(PolyLive *p, PolyFullBookPayload *out);
int poly_live_ws_done(PolyLive *p);

#endif /* POLY_LIVE_H */
