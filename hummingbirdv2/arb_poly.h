#ifndef ARB_POLY_H
#define ARB_POLY_H

#include <stdint.h>
#include "arb_ipc.h"

/*
 * Poly-side data structures and logic:
 * - full order book
 * - per-level volume tracking (10%/15% thresholds)
 * - binary presign pool (5 copies per power-of-two)
 * - hedge placement on Kalshi fills
 */

#define POLY_MAX_TRACKED  ARB_MAX_TRACKED
#define POLY_MAX_BITS     16  /* supports up to 2^15 sized slots */

typedef struct {
    double price;
    double size;
} PolyBookLevel;

typedef struct {
    /* local view of Poly order book (bids and asks, best first) */
    uint16_t n_bids;
    uint16_t n_asks;
    PolyBookLevel bids[ARB_MAX_LEVELS];
    PolyBookLevel asks[ARB_MAX_LEVELS];

    double best_bid; /* 0–1 */
    double best_ask; /* 0–1 */
} PolyState;

typedef struct {
    uint8_t in_use;
    uint8_t side;          /* Side enum */
    int16_t price_cents;   /* Kalshi YES price in cents */
    double  last_sent_vol; /* Poly volume last sent for this level */
} PolyTrackedLevel;

typedef struct {
    PolyTrackedLevel levels[POLY_MAX_TRACKED];
    uint8_t n_levels;
} PolyLevelTracker;

typedef struct {
    uint32_t size;   /* power-of-two contracts for this slot */
    uint8_t  copies; /* remaining presigned orders (0–5) */
} PolyPresignSlot;

typedef struct {
    PolyPresignSlot slots[POLY_MAX_BITS];
    uint8_t n_slots;
    uint64_t last_trade_ms; /* last time we consumed from pool */
} PolyPresignPool;

typedef struct {
    PolyState       state;
    PolyLevelTracker tracker_bid;
    PolyLevelTracker tracker_ask;
    PolyPresignPool presign_pool;
} PolyContext;

/* Initialization */
void poly_init_context(PolyContext *ctx);

/* Update local book from a full-book payload (Poly -> Kalshi message mirror). */
void poly_update_book_from_full(const PolyFullBookPayload *msg, PolyContext *ctx);

/* Configure tracked levels from Kalshi cascade description. */
void poly_setup_tracked_levels(PolyContext *ctx, const KalshiLevelsDonePayload *levels);

/*
 * Compute per-level volumes for tracked levels on one side and, if any
 * cross the 10%/15% thresholds, fill out 'out' and return 1. Otherwise
 * return 0 and leave 'out' untouched.
 */
int poly_compute_level_vol_update(PolyContext *ctx, Side side, PolyLevelVolUpdatePayload *out);

/* Handle a Kalshi fill by hedging on Poly using the binary presign pool. */
void poly_handle_kalshi_fill(PolyContext *ctx, const KalshiFillPayload *fill, uint64_t now_ms);

/* Allow the caller to run the 2s debounced resign check periodically. */
void poly_presign_maybe_resign(PolyContext *ctx, uint64_t now_ms);

/*
 * Poly process: WebSocket orderbook, send full book to Kalshi, track levels,
 * send volume updates (10%/15%), place CLOB hedge orders on Kalshi fills.
 */
void poly_process_run(int fd_in, int fd_out);

#endif /* ARB_POLY_H */


