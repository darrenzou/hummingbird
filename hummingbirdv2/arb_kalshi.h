#ifndef ARB_KALSHI_H
#define ARB_KALSHI_H

#include <stdint.h>
#include "arb_ipc.h"

/*
 * Kalshi-side data structures and cascade logic:
 * - local YES order book (bids / asks)
 * - portfolio caps and per-side budgeting
 * - multi-level cascading limit order computation
 * - reaction to Poly per-level volume updates (amend/cancel)
 */

#define KALSHI_MAX_LEVELS ARB_MAX_LEVELS

typedef struct {
    uint16_t n_yes_bids;
    uint16_t n_yes_asks;
    PriceLevel yes_bids[KALSHI_MAX_LEVELS];
    PriceLevel yes_asks[KALSHI_MAX_LEVELS];

    double kalshi_balance;  /* dollars */
    double poly_balance;    /* dollars */
    double side_cap;        /* max notional per side (0 = use default 1000) */

    int16_t top_bid_cascade_price; /* cents */
    int16_t top_ask_cascade_price; /* cents */
} KalshiState;

void kalshi_init_state(KalshiState *st);

void kalshi_set_portfolios(KalshiState *st, double kalshi_balance, double poly_balance);
void kalshi_set_side_cap(KalshiState *st, double cap);

void kalshi_update_orderbook(KalshiState *st,
                             const PriceLevel *yes_bids,
                             uint16_t n_bids,
                             const PriceLevel *yes_asks,
                             uint16_t n_asks);

/* Build cascading limit orders based on the Poly full book and Kalshi YES book. */
void kalshi_build_cascade(KalshiState *st,
                          const PolyFullBookPayload *poly_book,
                          KalshiLevelsDonePayload *out_levels);

/* React to Poly volume updates by conceptually amending/cancelling orders. */
void kalshi_apply_level_vol_update(KalshiState *st,
                                   const PolyLevelVolUpdatePayload *upd);

/*
 * Kalshi process: WebSocket orderbook, receive Poly full book, build cascade,
 * place/cancel/amend via REST, forward real fills to Poly.
 */
void kalshi_process_run(int fd_in, int fd_out);

#endif /* ARB_KALSHI_H */

