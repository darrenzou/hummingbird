#ifndef ARB_IPC_H
#define ARB_IPC_H

#include <stdint.h>

/*
 * Core IPC message types and shared data structures.
 *
 * This is intentionally self-contained and does not depend on any external
 * code from the original project. The goal is to capture the semantics of
 * the redesign: full Poly book, per-level volume updates, Kalshi cascade
 * levels, and fill notifications.
 */

typedef enum {
    MSG_NONE = 0,

    /* Poly -> Kalshi: full order book snapshot (bids and asks). */
    MSG_POLY_FULL_BOOK = 1,

    /* Poly -> Kalshi: per-level volume updates for tracked Kalshi prices. */
    MSG_POLY_LEVEL_VOL_UPDATE = 2,

    /* Kalshi -> Poly: cascade levels have been (re)computed and placed. */
    MSG_KALSHI_LEVELS_DONE = 3,

    /* Kalshi -> Poly: a limit order at a given level was filled. */
    MSG_KALSHI_FILL = 4,

    /* Poly -> Kalshi: abort (e.g. price out of band); Kalshi should cancel all open orders. */
    MSG_ABORT = 5,

    /* Kalshi -> Poly: cascade/place failed; Poly should abort (record and exit). */
    MSG_KALSHI_ABORT = 6
} ArbMsgType;

typedef enum {
    SIDE_BID = 0,
    SIDE_ASK = 1
} Side;

/* Limits chosen to keep ArbMsg a fixed, reasonably small size. */
#define ARB_MAX_LEVELS      64
#define ARB_MAX_TRACKED     32

typedef struct {
    double price;   /* Poly price 0–1 or Kalshi cents as needed. */
    double size;
} PriceLevel;

typedef struct {
    /* Poly -> Kalshi full book payload */
    uint16_t n_bids;
    uint16_t n_asks;
    PriceLevel bids[ARB_MAX_LEVELS];
    PriceLevel asks[ARB_MAX_LEVELS];
} PolyFullBookPayload;

typedef struct {
    /* Poly -> Kalshi per-level volume update payload */
    uint8_t side;                /* Side enum */
    uint8_t n_levels;
    int16_t price_cents[ARB_MAX_TRACKED];
    double  volume[ARB_MAX_TRACKED];
} PolyLevelVolUpdatePayload;

typedef struct {
    /* Kalshi -> Poly cascade levels description */
    uint8_t n_bid_levels;
    uint8_t n_ask_levels;
    int16_t bid_price_cents[ARB_MAX_TRACKED];
    double  bid_volume[ARB_MAX_TRACKED];
    int16_t ask_price_cents[ARB_MAX_TRACKED];
    double  ask_volume[ARB_MAX_TRACKED];
} KalshiLevelsDonePayload;

typedef struct {
    /* Kalshi -> Poly fill notification */
    uint8_t side;          /* Side enum */
    int16_t price_cents;   /* price of the level that filled (for logging) */
    uint32_t filled_count; /* contracts filled at this level */
} KalshiFillPayload;

typedef struct {
    ArbMsgType type;
    union {
        PolyFullBookPayload       poly_full_book;
        PolyLevelVolUpdatePayload poly_level_vol;
        KalshiLevelsDonePayload   kalshi_levels_done;
        KalshiFillPayload         kalshi_fill;
    } u;
} ArbMsg;

/*
 * Simple helper to initialize a message.
 */
static inline void arb_msg_init(ArbMsg *m, ArbMsgType t)
{
    if (!m) return;
    m->type = t;
}

/*
 * IPC helpers for fixed-size ArbMsg send/receive over a file descriptor.
 * arb_ipc_send/recv are blocking; return 0 on success, -1 on error or EOF.
 * arb_ipc_poll(fd, timeout_ms): 1 = data ready, 0 = timeout, -1 = error (for event loops).
 */
int arb_ipc_send(int fd, const ArbMsg *msg);
int arb_ipc_recv(int fd, ArbMsg *msg);
int arb_ipc_poll(int fd, int timeout_ms);

#endif /* ARB_IPC_H */

