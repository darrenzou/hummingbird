#ifndef ARB_DB_H
#define ARB_DB_H

#include <stdint.h>

/*
 * SQLite persistence for arbitrage events:
 * - On startup: store both orderbooks with action_type 'start'.
 * - On Kalshi fill: store both orderbooks + action (fill price, fill amount).
 * - On limit order resize: store both orderbooks + action (price point, volume before, volume after).
 *
 * Orderbook levels are stored per-event (snapshot at time of event).
 */

/* Opaque DB handle. */
struct arb_db;
typedef struct arb_db ArbDb;

/* Price/size for one level (matches arb_ipc PriceLevel). */
typedef struct {
    double price;
    double size;
} ArbDbLevel;

/*
 * Open or create DB at path. Returns 0 on success, -1 on error.
 * Creates tables if they do not exist.
 */
int arb_db_open(const char *path, ArbDb **out_db);

void arb_db_close(ArbDb *db);

/*
 * Record a start event: both orderbooks only (action_type = 'start').
 */
int arb_db_record_start(ArbDb *db,
                       const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                       const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                       const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                       const ArbDbLevel *poly_asks, uint16_t n_poly_asks);

/*
 * Record a fill event: both orderbooks + fill price (cents) and fill amount.
 */
int arb_db_record_fill(ArbDb *db,
                      const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                      const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                      const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                      const ArbDbLevel *poly_asks, uint16_t n_poly_asks,
                      int fill_price_cents, uint32_t fill_amount);

/*
 * Record a resize event: both orderbooks + price point (cents), side ('bid'/'ask'),
 * volume before and volume after.
 */
int arb_db_record_resize(ArbDb *db,
                         const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                         const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                         const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                         const ArbDbLevel *poly_asks, uint16_t n_poly_asks,
                         int price_cents, int is_bid,
                         double vol_before, double vol_after);

/*
 * Record an abort event (e.g. from Poly: price out of band or hedge order error).
 * reason is stored in events.abort_reason (action_type = 'abort').
 */
int arb_db_record_abort(ArbDb *db, const char *reason);

#endif /* ARB_DB_H */
