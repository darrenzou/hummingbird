/*
 * arb_ipc.h
 *
 * Inter-process communication protocol between the Polymarket process
 * (parent, after fork) and the Kalshi process (child).
 *
 * Messages are fixed-size binary structs written/read over a pair of
 * unidirectional pipes:
 *
 *   pipe A  Poly → Kalshi
 *   pipe B  Kalshi → Poly
 *
 * POSIX guarantees atomic writes of ≤ PIPE_BUF bytes, so there is no
 * message framing needed (sizeof(ArbMsg) is well below 4096 bytes).
 */
#pragma once

#include <unistd.h>
#include <stdint.h>

/* ── Message types ────────────────────────────────────────────────────────── */

typedef enum {
    MSG_POLY_BOOK          = 1,  /* Poly  → Kalshi : initial book snapshot    */
    MSG_KALSHI_SIGNAL      = 2,  /* Kalshi → Poly  : arb check result         */
    MSG_POLY_SIGNING_DONE  = 3,  /* Poly  → Kalshi : 20 orders pre-signed     */
    MSG_POLY_REDO_SIGNING  = 4,  /* Kalshi → Poly  : prices changed, redo     */
    MSG_KALSHI_FILL        = 5,  /* Kalshi → Poly  : limit order fill         */
    MSG_ABORT              = 6,  /* Either → other : abort program            */
    MSG_POLY_PRICE_UPDATE  = 7,  /* Poly  → Kalshi : best bid/ask changed     */
    MSG_POLY_VOL_UPDATE    = 8,  /* Poly  → Kalshi : best-vol drop >15%%       */
} ArbMsgType;

/* ── Message payload ──────────────────────────────────────────────────────── */

typedef struct {
    ArbMsgType type;

    union {
        /*
         * MSG_POLY_BOOK
         * Sent once after Poly receives the initial WS book snapshot.
         * bid/ask are in Poly's native 0–1 scale (e.g. 0.48 = 48 cents).
         * bid_vol / ask_vol are in token units (e.g. 100.0 = 100 tokens).
         */
        struct {
            double bid;
            double ask;
            double bid_vol;
            double ask_vol;
        } poly_book;

        /*
         * MSG_KALSHI_SIGNAL
         * Sent by Kalshi after the arb check.
         *   bid_ok == 1  => bid leg profitable  (poly_bid > kalshi_bid)
         *   ask_ok == 1  => ask leg profitable  (poly_ask < kalshi_ask)
         * At least one will be 1; if both are 0 Poly should abort.
         * kalshi_bid/ask are integer cents (1-99).
         */
        struct {
            int bid_ok;
            int ask_ok;
            int kalshi_bid;
            int kalshi_ask;
        } kalshi_signal;

        /*
         * MSG_POLY_SIGNING_DONE
         * Sent after Poly finishes pre-signing 20 orders.
         * Carries the book volumes so Kalshi can size its limit orders.
         * Volumes are in tokens (same units as poly_book.bid_vol).
         * poly_balance is the Polymarket USDC available balance (in whole
         * dollars) at the time of signing; used by Kalshi to cap order size
         * when sufficient cash is available to scale beyond 75% of top-of-book.
         */
        struct {
            double poly_bid_vol;
            double poly_ask_vol;
            double poly_balance;   /* USDC available balance, in dollars */
        } poly_signing_done;

        /*
         * MSG_POLY_REDO_SIGNING
         * Sent by Kalshi when the orderbook moved between Poly sending
         * POLY_BOOK and receiving POLY_SIGNING_DONE.
         * Poly must re-sign with the updated prices.
         */
        struct {
            int kalshi_bid;
            int kalshi_ask;
        } redo_signing;

        /*
         * MSG_KALSHI_FILL
         * Sent when one of Kalshi's resting limit orders is (partially)
         * filled.  Poly uses filled_count + is_bid to decide how many
         * pre-signed orders to place.
         */
        struct {
            double filled_count;    /* contracts filled on this notification */
            int    is_bid;          /* 1 = bid order filled, 0 = ask        */
            char   order_id[64];    /* Kalshi order ID (for logging)         */
        } kalshi_fill;

        /*
         * MSG_ABORT
         *   reason 0 = no arb opportunity
         *   reason 1 = price outside 5–95% threshold
         *   reason 2 = pipe/IPC error
         */
        struct {
            int reason;
        } abort_msg;

        /*
         * MSG_POLY_PRICE_UPDATE
         * Sent by Poly whenever its best bid or ask changes during the
         * monitoring loop.  Kalshi uses this to detect when the spread
         * is narrowing and may place additional limit orders.
         * bid/ask are in Poly’s native 0–1 scale (e.g. 0.48 = 48 cents).
         */
        struct {
            double bid;
            double ask;
        } poly_price_update;

        /*
         * MSG_POLY_VOL_UPDATE
         * Sent when Polymarket best-bid or best-ask volume drops >15%%
         * from the value last reported to Kalshi.  Kalshi should amend
         * its resting orders downward to reflect reduced liquidity.
         * bid_vol / ask_vol are current best-vol in token units.
         */
        struct {
            double bid_vol;
            double ask_vol;
        } poly_vol_update;

    } d;
} ArbMsg;

/* ── Transport helpers ────────────────────────────────────────────────────── */

/*
 * ipc_send – write one ArbMsg to the pipe (blocking).
 * Returns 1 on success, 0 on write error.
 */
int ipc_send(int fd, const ArbMsg *msg);

/*
 * ipc_recv – read one ArbMsg from the pipe (blocking).
 * Returns 1 on success, 0 on EOF/error.
 */
int ipc_recv(int fd, ArbMsg *msg);

/*
 * ipc_recv_nb – non-blocking read using select(timeout=0).
 * Returns  1 : message received
 *          0 : no data available (would block)
 *         -1 : EOF or error
 */
int ipc_recv_nb(int fd, ArbMsg *msg);
