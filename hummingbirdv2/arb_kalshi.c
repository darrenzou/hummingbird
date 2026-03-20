#define _POSIX_C_SOURCE 200809L
#include "arb_kalshi.h"
#include "arb_db.h"
#include "arb_config.h"
#include "kalshi_live.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define KALSHI_429_RETRIES 5
#define MODIFY_RETRIES 5
#define POSITION_REBALANCE_DEBOUNCE_MS 2000

static double poly_best_bid(const PolyFullBookPayload *book)
{
    if (!book || book->n_bids == 0) return 0.0;
    return book->bids[0].price;
}

static double poly_best_ask(const PolyFullBookPayload *book)
{
    if (!book || book->n_asks == 0) return 1.0;
    return book->asks[0].price;
}

static double poly_vol_above_from_book(const PolyFullBookPayload *book, int price_cents)
{
    if (!book) return 0.0;
    double threshold = price_cents / 100.0;
    double vol = 0.0;
    for (uint16_t i = 0; i < book->n_bids; ++i) {
        if (book->bids[i].price > threshold) {
            vol += book->bids[i].size;
        }
    }
    return vol;
}

static double poly_vol_below_from_book(const PolyFullBookPayload *book, int price_cents)
{
    if (!book) return 0.0;
    double threshold = price_cents / 100.0;
    double vol = 0.0;
    for (uint16_t i = 0; i < book->n_asks; ++i) {
        if (book->asks[i].price < threshold) {
            vol += book->asks[i].size;
        }
    }
    return vol;
}

void kalshi_init_state(KalshiState *st)
{
    if (!st) return;
    st->n_yes_bids = 0;
    st->n_yes_asks = 0;
    st->kalshi_balance = 0.0;
    st->poly_balance = 0.0;
    st->side_cap = 1000.0;
    st->top_bid_cascade_price = -1;
    st->top_ask_cascade_price = -1;
}

void kalshi_set_portfolios(KalshiState *st, double kalshi_balance, double poly_balance)
{
    if (!st) return;
    st->kalshi_balance = kalshi_balance;
    st->poly_balance = poly_balance;
}

void kalshi_set_side_cap(KalshiState *st, double cap)
{
    if (!st) return;
    st->side_cap = (cap > 0.0) ? cap : 1000.0;
}

void kalshi_update_orderbook(KalshiState *st,
                             const PriceLevel *yes_bids,
                             uint16_t n_bids,
                             const PriceLevel *yes_asks,
                             uint16_t n_asks)
{
    if (!st) return;
    st->n_yes_bids = (n_bids > KALSHI_MAX_LEVELS) ? KALSHI_MAX_LEVELS : n_bids;
    st->n_yes_asks = (n_asks > KALSHI_MAX_LEVELS) ? KALSHI_MAX_LEVELS : n_asks;

    for (uint16_t i = 0; i < st->n_yes_bids; ++i) {
        st->yes_bids[i] = yes_bids[i];
    }
    for (uint16_t i = 0; i < st->n_yes_asks; ++i) {
        st->yes_asks[i] = yes_asks[i];
    }
}

void kalshi_build_cascade(KalshiState *st,
                          const PolyFullBookPayload *poly_book,
                          KalshiLevelsDonePayload *out_levels)
{
    if (!st || !poly_book || !out_levels) return;

    out_levels->n_bid_levels = 0;
    out_levels->n_ask_levels = 0;

    double portfolio = st->kalshi_balance;
    if (st->poly_balance < portfolio) portfolio = st->poly_balance;

    double bid_budget = 0.5 * portfolio;
    double ask_budget = 0.5 * portfolio;
    double cap = (st->side_cap > 0.0) ? st->side_cap : 1000.0;
    if (bid_budget > cap) bid_budget = cap;
    if (ask_budget > cap) ask_budget = cap;

    /* Bid side cascade */
    double p_bid = poly_best_bid(poly_book);
    int P_bid = (int)(p_bid * 100.0 + 0.5);
    double sum_prev_bid = 0.0;
    int first_bid_set = 0;

    for (uint16_t i = 0; i < st->n_yes_bids && out_levels->n_bid_levels < ARB_MAX_TRACKED; ++i) {
        int k_bid = (int)(st->yes_bids[i].price + 0.5); /* assume cents in price */
        double k_qty = st->yes_bids[i].size;

        if (k_bid >= P_bid) {
            continue; /* not at least 1c cheaper than Poly */
        }
        if (!first_bid_set && k_bid > P_bid - 1) {
            continue;
        }

        if (!first_bid_set) {
            first_bid_set = 1;
            st->top_bid_cascade_price = (int16_t)k_bid;
        }

        double poly_vol = poly_vol_above_from_book(poly_book, k_bid);
        double available_from_poly = poly_vol * 0.75 - sum_prev_bid;
        if (available_from_poly <= 0.0) {
            break;
        }

        double requested = k_qty / 2.0;
        if (requested > available_from_poly) requested = available_from_poly;
        if (requested <= 0.0) {
            break;
        }

        /* Budget cap for this side: compute max contracts we can afford. */
        double price_dollars = k_bid / 100.0;
        if (price_dollars <= 0.0) {
            break;
        }
        double max_by_budget = bid_budget / price_dollars;
        if (max_by_budget <= 0.0) {
            break; /* budget exhausted */
        }
        if (requested > max_by_budget) {
            requested = max_by_budget;
        }

        if (requested <= 0.0) {
            break;
        }

        sum_prev_bid += requested;
        bid_budget -= requested * price_dollars;
        if (bid_budget < 0.0) bid_budget = 0.0;

        out_levels->bid_price_cents[out_levels->n_bid_levels] = (int16_t)k_bid;
        out_levels->bid_volume[out_levels->n_bid_levels] = requested;
        ++out_levels->n_bid_levels;

        printf("[kalshi] bid cascade level: price=%d vol=%.2f\n", k_bid, requested);

        if (bid_budget <= 0.0) {
            break;
        }
    }

    /* Ask side cascade */
    double p_ask = poly_best_ask(poly_book);
    int P_ask = (int)(p_ask * 100.0 + 0.5);
    double sum_prev_ask = 0.0;
    int first_ask_set = 0;

    for (uint16_t i = 0; i < st->n_yes_asks && out_levels->n_ask_levels < ARB_MAX_TRACKED; ++i) {
        int k_ask = (int)(st->yes_asks[i].price + 0.5);
        double k_qty = st->yes_asks[i].size;

        if (k_ask < P_ask + 1) {
            continue; /* not at least 1c above Poly best ask */
        }
        if (!first_ask_set && k_ask < P_ask + 1) {
            continue;
        }

        if (!first_ask_set) {
            first_ask_set = 1;
            st->top_ask_cascade_price = (int16_t)k_ask;
        }

        double poly_vol = poly_vol_below_from_book(poly_book, k_ask);
        double available_from_poly = poly_vol * 0.75 - sum_prev_ask;
        if (available_from_poly <= 0.0) {
            break;
        }

        double requested = k_qty / 2.0;
        if (requested > available_from_poly) requested = available_from_poly;
        if (requested <= 0.0) {
            break;
        }

        double price_dollars = k_ask / 100.0;
        if (price_dollars <= 0.0) {
            break;
        }
        double max_by_budget = ask_budget / price_dollars;
        if (max_by_budget <= 0.0) {
            break;
        }
        if (requested > max_by_budget) {
            requested = max_by_budget;
        }
        if (requested <= 0.0) {
            break;
        }

        sum_prev_ask += requested;
        ask_budget -= requested * price_dollars;
        if (ask_budget < 0.0) ask_budget = 0.0;

        out_levels->ask_price_cents[out_levels->n_ask_levels] = (int16_t)k_ask;
        out_levels->ask_volume[out_levels->n_ask_levels] = requested;
        ++out_levels->n_ask_levels;

        printf("[kalshi] ask cascade level: price=%d vol=%.2f\n", k_ask, requested);

        if (ask_budget <= 0.0) {
            break;
        }
    }
}

/* Per-level volume tracking for resize events (vol_before / vol_after). */
typedef struct {
    int16_t price_cents;
    double vol;
} LevelVolTrack;

void kalshi_apply_level_vol_update(KalshiState *st,
                                   const PolyLevelVolUpdatePayload *upd)
{
    if (!st || !upd) return;

    const char *side_str = (upd->side == SIDE_BID) ? "BID" : "ASK";
    printf("[kalshi] volume update for side=%s, n_levels=%u\n",
           side_str, upd->n_levels);

    for (uint8_t i = 0; i < upd->n_levels; ++i) {
        int price_cents = upd->price_cents[i];
        double vol = upd->volume[i];
        if (vol <= 0.0) {
            printf("  cancel level price=%d (new vol=0)\n", price_cents);
        } else {
            printf("  amend level price=%d to vol=%.2f\n", price_cents, vol);
        }
    }
}

/* Real Kalshi WebSocket + REST; orderbook from WS, place/cancel/amend via REST. */
static void kalshi_run_live(int fd_in, int fd_out)
{
    ArbCreds creds;
    arb_load_creds(&creds);
    const char *ticker = getenv("ARB_TICKER");
    if (!ticker || !*ticker || !creds.kalshi_api_key_id[0] || !creds.kalshi_private_key_pem[0]) {
        fprintf(stderr, "[kalshi] live mode requires ARB_TICKER and Kalshi creds in .env\n");
        return;
    }

    KalshiLive *kl = kalshi_live_create(&creds, ticker);
    if (!kl) {
        fprintf(stderr, "[kalshi] kalshi_live_create failed\n");
        return;
    }
    if (!kalshi_live_ws_connect(kl)) {
        fprintf(stderr, "[kalshi] WebSocket connect failed\n");
        kalshi_live_destroy(kl);
        return;
    }

    KalshiState ks;
    kalshi_init_state(&ks);
    double kb = kalshi_live_get_balance(kl);
    double pb = 2000.0;
    const char *v = getenv("ARB_POLY_BALANCE");
    if (v && *v) { pb = atof(v); if (pb < 0) pb = 2000.0; }
    double sc = 1000.0;
    if ((v = getenv("ARB_SIDE_CAP")) && *v) { sc = atof(v); if (sc <= 0) sc = 1000.0; }
    kalshi_set_portfolios(&ks, kb, pb);
    kalshi_set_side_cap(&ks, sc);

    double yb[ARB_MAX_LEVELS], bs[ARB_MAX_LEVELS], ya[ARB_MAX_LEVELS], as[ARB_MAX_LEVELS];
    int nb = 0, na = 0;
    kalshi_live_ws_copy_orderbook(kl, yb, bs, &nb, ya, as, &na, ARB_MAX_LEVELS);
    ks.n_yes_bids = (uint16_t)nb;
    ks.n_yes_asks = (uint16_t)na;
    for (int i = 0; i < nb; i++) {
        ks.yes_bids[i].price = yb[i];
        ks.yes_bids[i].size = bs[i];
    }
    for (int i = 0; i < na; i++) {
        ks.yes_asks[i].price = ya[i];
        ks.yes_asks[i].size = as[i];
    }

    ArbMsg msg;
    if (arb_ipc_recv(fd_in, &msg) != 0 || msg.type != MSG_POLY_FULL_BOOK) {
        fprintf(stderr, "[kalshi] expected POLY_FULL_BOOK\n");
        kalshi_live_destroy(kl);
        return;
    }
    PolyFullBookPayload last_poly_book;
    memcpy(&last_poly_book, &msg.u.poly_full_book, sizeof(last_poly_book));

    ArbDb *db = NULL;
    const char *db_path = getenv("ARB_DB_PATH");
    if (!db_path) db_path = "arb_events.db";
    if (arb_db_open(db_path, &db) == 0) {
        arb_db_record_start(db,
                            (const ArbDbLevel *)ks.yes_bids, ks.n_yes_bids,
                            (const ArbDbLevel *)ks.yes_asks, ks.n_yes_asks,
                            (const ArbDbLevel *)last_poly_book.bids, last_poly_book.n_bids,
                            (const ArbDbLevel *)last_poly_book.asks, last_poly_book.n_asks);
    }

    KalshiLevelsDonePayload levels_msg;
    kalshi_build_cascade(&ks, &last_poly_book, &levels_msg);

    char bid_oid[ARB_MAX_TRACKED][64];
    char ask_oid[ARB_MAX_TRACKED][64];
    memset(bid_oid, 0, sizeof(bid_oid));
    memset(ask_oid, 0, sizeof(ask_oid));
    ArbMsg out;

    /* Batch place bid levels (up to KALSHI_BATCH_MAX per request). 429: retry 5 times; other error: abort to Poly. */
    KalshiBatchOrder batch[KALSHI_BATCH_MAX];
    char oids[KALSHI_BATCH_MAX][64];
    int bid_idx = 0;
    while (bid_idx < (int)levels_msg.n_bid_levels) {
        int n = 0;
        int level_map[KALSHI_BATCH_MAX];
        for (; bid_idx < (int)levels_msg.n_bid_levels && n < KALSHI_BATCH_MAX; bid_idx++) {
            int count = (int)levels_msg.bid_volume[bid_idx];
            if (count <= 0) continue;
            batch[n].action = "buy";
            batch[n].count = count;
            batch[n].yes_price = (int)levels_msg.bid_price_cents[bid_idx] + 1;
            batch[n].client_order_id[0] = '\0';
            level_map[n] = bid_idx;
            n++;
        }
        if (n == 0) break;
        long http_status = 0;
        int placed = -1;
        for (int retry = 0; retry <= KALSHI_429_RETRIES; retry++) {
            placed = kalshi_live_batch_place_orders(kl, batch, n, oids, &http_status);
            if (placed > 0) break;
            if (placed == 0 && http_status == 429 && retry < KALSHI_429_RETRIES) {
                struct timespec ts = { 1, 0 };
                nanosleep(&ts, NULL);
                continue;
            }
            arb_msg_init(&out, MSG_KALSHI_ABORT);
            arb_ipc_send(fd_out, &out);
            fprintf(stderr, "[kalshi] cascade batch failed (http=%ld) -> abort to Poly\n", http_status);
            goto cascade_failed;
        }
        for (int i = 0; i < n; i++)
            if (oids[i][0])
                strncpy(bid_oid[level_map[i]], oids[i], 63);
    }
    /* Batch place ask levels. */
    int ask_idx = 0;
    while (ask_idx < (int)levels_msg.n_ask_levels) {
        int n = 0;
        int level_map[KALSHI_BATCH_MAX];
        for (; ask_idx < (int)levels_msg.n_ask_levels && n < KALSHI_BATCH_MAX; ask_idx++) {
            int count = (int)levels_msg.ask_volume[ask_idx];
            if (count <= 0) continue;
            batch[n].action = "sell";
            batch[n].count = count;
            batch[n].yes_price = (int)levels_msg.ask_price_cents[ask_idx];
            batch[n].client_order_id[0] = '\0';
            level_map[n] = ask_idx;
            n++;
        }
        if (n == 0) break;
        long http_status = 0;
        int placed = -1;
        for (int retry = 0; retry <= KALSHI_429_RETRIES; retry++) {
            placed = kalshi_live_batch_place_orders(kl, batch, n, oids, &http_status);
            if (placed > 0) break;
            if (placed == 0 && http_status == 429 && retry < KALSHI_429_RETRIES) {
                struct timespec ts = { 1, 0 };
                nanosleep(&ts, NULL);
                continue;
            }
            arb_msg_init(&out, MSG_KALSHI_ABORT);
            arb_ipc_send(fd_out, &out);
            fprintf(stderr, "[kalshi] cascade batch failed (http=%ld) -> abort to Poly\n", http_status);
            goto cascade_failed;
        }
        for (int i = 0; i < n; i++)
            if (oids[i][0])
                strncpy(ask_oid[level_map[i]], oids[i], 63);
    }

    arb_msg_init(&out, MSG_KALSHI_LEVELS_DONE);
    out.u.kalshi_levels_done = levels_msg;
    arb_ipc_send(fd_out, &out);
    kalshi_live_ws_subscribe_fills(kl);

    LevelVolTrack last_bid_levels[ARB_MAX_TRACKED], last_ask_levels[ARB_MAX_TRACKED];
    int n_last_bid = 0, n_last_ask = 0;
    memset(last_bid_levels, 0, sizeof(last_bid_levels));
    memset(last_ask_levels, 0, sizeof(last_ask_levels));
    for (uint8_t i = 0; i < levels_msg.n_bid_levels; i++) {
        last_bid_levels[n_last_bid].price_cents = levels_msg.bid_price_cents[i];
        last_bid_levels[n_last_bid].vol = levels_msg.bid_volume[i];
        n_last_bid++;
    }
    for (uint8_t i = 0; i < levels_msg.n_ask_levels; i++) {
        last_ask_levels[n_last_ask].price_cents = levels_msg.ask_price_cents[i];
        last_ask_levels[n_last_ask].vol = levels_msg.ask_volume[i];
        n_last_ask++;
    }

    /* One modify/cancel per order in transit: skip if pending for that level. */
    int bid_pending[ARB_MAX_TRACKED];
    int ask_pending[ARB_MAX_TRACKED];
    memset(bid_pending, 0, sizeof(bid_pending));
    memset(ask_pending, 0, sizeof(ask_pending));

    /* Position rebalance: track net YES contracts, allocate to opposite side after debounce. */
    int kalshi_yes_position = 0;
    int last_rebalanced_position = 0;
    uint64_t last_fill_ms = 0;

    /* Level index + price for sorting volume updates by cascade priority. */
    typedef struct { int j; int16_t price_cents; double vol; } LevelUpdate;
    LevelUpdate sorted[ARB_MAX_TRACKED];

    int iter = 0;
    while (!kalshi_live_ws_done(kl) && iter < 12000) {
        iter++;
        if (arb_ipc_poll(fd_in, 50) == 1) {
            if (arb_ipc_recv(fd_in, &msg) != 0) continue;
            if (msg.type == MSG_ABORT) {
                /* Mass cancel all open orders, cascade order (best first). Retry each cancel up to 5 times. */
                #define ABORT_RETRIES 5
                for (int j = 0; j < n_last_bid; j++) {
                    if (!bid_oid[j][0]) continue;
                    for (int r = 0; r < ABORT_RETRIES; r++) {
                        if (kalshi_live_cancel_order(kl, bid_oid[j])) {
                            bid_oid[j][0] = '\0';
                            break;
                        }
                        if (r == ABORT_RETRIES - 1)
                            fprintf(stderr, "[kalshi] abort: cancel bid order %s failed after %d retries\n", bid_oid[j], ABORT_RETRIES);
                    }
                }
                for (int j = 0; j < n_last_ask; j++) {
                    if (!ask_oid[j][0]) continue;
                    for (int r = 0; r < ABORT_RETRIES; r++) {
                        if (kalshi_live_cancel_order(kl, ask_oid[j])) {
                            ask_oid[j][0] = '\0';
                            break;
                        }
                        if (r == ABORT_RETRIES - 1)
                            fprintf(stderr, "[kalshi] abort: cancel ask order %s failed after %d retries\n", ask_oid[j], ABORT_RETRIES);
                    }
                }
                printf("[kalshi] abort: mass cancel done\n");
                continue;
            }
            if (msg.type == MSG_POLY_LEVEL_VOL_UPDATE) {
                const PolyLevelVolUpdatePayload *upd = &msg.u.poly_level_vol;
                int n_sorted = 0;
                if (upd->side == SIDE_BID) {
                    for (uint8_t i = 0; i < upd->n_levels && n_sorted < ARB_MAX_TRACKED; i++) {
                        int pc = upd->price_cents[i];
                        double nv = upd->volume[i];
                        for (int j = 0; j < n_last_bid; j++) {
                            if (last_bid_levels[j].price_cents == pc) {
                                sorted[n_sorted].j = j;
                                sorted[n_sorted].price_cents = (int16_t)pc;
                                sorted[n_sorted].vol = nv;
                                n_sorted++;
                                break;
                            }
                        }
                    }
                    /* Sort by cascade priority: best bid first (highest price_cents). */
                    for (int a = 0; a < n_sorted; a++)
                        for (int b = a + 1; b < n_sorted; b++)
                            if (sorted[b].price_cents > sorted[a].price_cents) {
                                LevelUpdate t = sorted[a];
                                sorted[a] = sorted[b];
                                sorted[b] = t;
                            }
                    for (int si = 0; si < n_sorted; si++) {
                        int j = sorted[si].j;
                        int pc = (int)sorted[si].price_cents;
                        double nv = sorted[si].vol;
                        if (si == 0 && kalshi_yes_position < 0 &&
                            (uint64_t)arb_now_ms() - last_fill_ms > POSITION_REBALANCE_DEBOUNCE_MS) {
                            nv += (double)(-kalshi_yes_position);
                            last_rebalanced_position = kalshi_yes_position;
                            printf("[kalshi] position rebalance: add %d to top bid (position=%d)\n",
                                   -kalshi_yes_position, kalshi_yes_position);
                        }
                        if (bid_pending[j]) continue;
                        bid_pending[j] = 1;
                        double vol_before = last_bid_levels[j].vol;
                        if (nv <= 0 && bid_oid[j][0]) {
                            (void)kalshi_live_cancel_order(kl, bid_oid[j]);
                            bid_oid[j][0] = '\0';
                        } else if (bid_oid[j][0]) {
                            int amend_ok = 0;
                            for (int r = 0; r < MODIFY_RETRIES && !amend_ok; r++) {
                                amend_ok = kalshi_live_amend_order(kl, bid_oid[j], "yes", "buy", pc + 1, (int)nv);
                                if (!amend_ok && r < MODIFY_RETRIES - 1) {
                                    struct timespec ts = { 1, 0 };
                                    nanosleep(&ts, NULL);
                                }
                            }
                            if (!amend_ok) {
                                fprintf(stderr, "[kalshi] modify bid order failed after %d retries -> abort, cancel all orders\n", MODIFY_RETRIES);
                                goto modify_failed;
                            }
                        }
                        bid_pending[j] = 0;
                        last_bid_levels[j].vol = nv;
                        if (db) arb_db_record_resize(db,
                            (const ArbDbLevel *)ks.yes_bids, ks.n_yes_bids,
                            (const ArbDbLevel *)ks.yes_asks, ks.n_yes_asks,
                            (const ArbDbLevel *)last_poly_book.bids, last_poly_book.n_bids,
                            (const ArbDbLevel *)last_poly_book.asks, last_poly_book.n_asks,
                            pc, 1, vol_before, nv);
                    }
                } else {
                    for (uint8_t i = 0; i < upd->n_levels && n_sorted < ARB_MAX_TRACKED; i++) {
                        int pc = upd->price_cents[i];
                        double nv = upd->volume[i];
                        for (int j = 0; j < n_last_ask; j++) {
                            if (last_ask_levels[j].price_cents == pc) {
                                sorted[n_sorted].j = j;
                                sorted[n_sorted].price_cents = (int16_t)pc;
                                sorted[n_sorted].vol = nv;
                                n_sorted++;
                                break;
                            }
                        }
                    }
                    /* Sort by cascade priority: best ask first (lowest price_cents). */
                    for (int a = 0; a < n_sorted; a++)
                        for (int b = a + 1; b < n_sorted; b++)
                            if (sorted[b].price_cents < sorted[a].price_cents) {
                                LevelUpdate t = sorted[a];
                                sorted[a] = sorted[b];
                                sorted[b] = t;
                            }
                    for (int si = 0; si < n_sorted; si++) {
                        int j = sorted[si].j;
                        int pc = (int)sorted[si].price_cents;
                        double nv = sorted[si].vol;
                        if (si == 0 && kalshi_yes_position > 0 &&
                            (uint64_t)arb_now_ms() - last_fill_ms > POSITION_REBALANCE_DEBOUNCE_MS) {
                            nv += (double)kalshi_yes_position;
                            last_rebalanced_position = kalshi_yes_position;
                            printf("[kalshi] position rebalance: add %d to top ask (position=%d)\n",
                                   kalshi_yes_position, kalshi_yes_position);
                        }
                        if (ask_pending[j]) continue;
                        ask_pending[j] = 1;
                        double vol_before = last_ask_levels[j].vol;
                        if (nv <= 0 && ask_oid[j][0]) {
                            (void)kalshi_live_cancel_order(kl, ask_oid[j]);
                            ask_oid[j][0] = '\0';
                        } else if (ask_oid[j][0]) {
                            int amend_ok = 0;
                            for (int r = 0; r < MODIFY_RETRIES && !amend_ok; r++) {
                                amend_ok = kalshi_live_amend_order(kl, ask_oid[j], "yes", "sell", pc, (int)nv);
                                if (!amend_ok && r < MODIFY_RETRIES - 1) {
                                    struct timespec ts = { 1, 0 };
                                    nanosleep(&ts, NULL);
                                }
                            }
                            if (!amend_ok) {
                                fprintf(stderr, "[kalshi] modify ask order failed after %d retries -> abort, cancel all orders\n", MODIFY_RETRIES);
                                goto modify_failed;
                            }
                        }
                        ask_pending[j] = 0;
                        last_ask_levels[j].vol = nv;
                        if (db) arb_db_record_resize(db,
                            (const ArbDbLevel *)ks.yes_bids, ks.n_yes_bids,
                            (const ArbDbLevel *)ks.yes_asks, ks.n_yes_asks,
                            (const ArbDbLevel *)last_poly_book.bids, last_poly_book.n_bids,
                            (const ArbDbLevel *)last_poly_book.asks, last_poly_book.n_asks,
                            pc, 0, vol_before, nv);
                    }
                }
            }
        }
        kalshi_live_ws_service(kl, 50);

        /* Timer-based position rebalance: amend top level when no Poly update arrives. */
        if (kalshi_yes_position != last_rebalanced_position &&
            (uint64_t)arb_now_ms() - last_fill_ms > POSITION_REBALANCE_DEBOUNCE_MS &&
            last_fill_ms != 0) {
            if (kalshi_yes_position > 0 && n_last_ask > 0 && ask_oid[0][0]) {
                int add_vol = kalshi_yes_position;
                double base = last_ask_levels[0].vol - (double)(last_rebalanced_position > 0 ? last_rebalanced_position : 0);
                int new_vol = (int)(base + (double)add_vol);
                if (new_vol > 0) {
                    int amend_ok = kalshi_live_amend_order(kl, ask_oid[0], "yes", "sell",
                                                          (int)last_ask_levels[0].price_cents, new_vol);
                    if (amend_ok) {
                        last_rebalanced_position = kalshi_yes_position;
                        last_ask_levels[0].vol = (double)new_vol;
                        printf("[kalshi] position rebalance (timer): add %d to top ask -> vol=%d\n",
                               add_vol, new_vol);
                    }
                }
            } else if (kalshi_yes_position < 0 && n_last_bid > 0 && bid_oid[0][0]) {
                int add_vol = -kalshi_yes_position;
                double base = last_bid_levels[0].vol - (double)(last_rebalanced_position < 0 ? -last_rebalanced_position : 0);
                int new_vol = (int)(base + (double)add_vol);
                if (new_vol > 0) {
                    int amend_ok = kalshi_live_amend_order(kl, bid_oid[0], "yes", "buy",
                                                          (int)last_bid_levels[0].price_cents + 1, new_vol);
                    if (amend_ok) {
                        last_rebalanced_position = kalshi_yes_position;
                        last_bid_levels[0].vol = (double)new_vol;
                        printf("[kalshi] position rebalance (timer): add %d to top bid -> vol=%d\n",
                               add_vol, new_vol);
                    }
                }
            }
        }

        uint32_t fc;
        int is_bid;
        char oid[64];
        if (kalshi_live_ws_poll_fill(kl, &fc, &is_bid, oid, sizeof(oid))) {
            int price_cents = 0;
            if (is_bid) {
                for (int j = 0; j < n_last_bid; j++)
                    if (strcmp(bid_oid[j], oid) == 0) { price_cents = last_bid_levels[j].price_cents + 1; break; }
            } else {
                for (int j = 0; j < n_last_ask; j++)
                    if (strcmp(ask_oid[j], oid) == 0) { price_cents = last_ask_levels[j].price_cents; break; }
            }
            if (db) {
                kalshi_live_ws_copy_orderbook(kl, yb, bs, &nb, ya, as, &na, ARB_MAX_LEVELS);
                ks.n_yes_bids = (uint16_t)nb;
                ks.n_yes_asks = (uint16_t)na;
                for (int i = 0; i < nb; i++) { ks.yes_bids[i].price = yb[i]; ks.yes_bids[i].size = bs[i]; }
                for (int i = 0; i < na; i++) { ks.yes_asks[i].price = ya[i]; ks.yes_asks[i].size = as[i]; }
                arb_db_record_fill(db,
                                  (const ArbDbLevel *)ks.yes_bids, ks.n_yes_bids,
                                  (const ArbDbLevel *)ks.yes_asks, ks.n_yes_asks,
                                  (const ArbDbLevel *)last_poly_book.bids, last_poly_book.n_bids,
                                  (const ArbDbLevel *)last_poly_book.asks, last_poly_book.n_asks,
                                  price_cents, fc);
            }
            KalshiFillPayload fill;
            fill.side = (uint8_t)(is_bid ? SIDE_BID : SIDE_ASK);
            fill.price_cents = (int16_t)price_cents;
            fill.filled_count = fc;
            arb_msg_init(&out, MSG_KALSHI_FILL);
            out.u.kalshi_fill = fill;
            arb_ipc_send(fd_out, &out);

            kalshi_yes_position += is_bid ? (int)fc : -(int)fc;
            last_fill_ms = (uint64_t)arb_now_ms();
            printf("[kalshi] fill %s count=%u -> sent to Poly (position=%d)\n",
                   is_bid ? "bid" : "ask", (unsigned)fc, kalshi_yes_position);
        }
    }
modify_failed:
    /* Cancel all open orders then send abort to Poly and exit. */
    for (int j = 0; j < n_last_bid; j++) {
        if (!bid_oid[j][0]) continue;
        for (int r = 0; r < ABORT_RETRIES; r++) {
            if (kalshi_live_cancel_order(kl, bid_oid[j])) {
                bid_oid[j][0] = '\0';
                break;
            }
            if (r == ABORT_RETRIES - 1)
                fprintf(stderr, "[kalshi] abort: cancel bid order %s failed after %d retries\n", bid_oid[j], ABORT_RETRIES);
        }
    }
    for (int j = 0; j < n_last_ask; j++) {
        if (!ask_oid[j][0]) continue;
        for (int r = 0; r < ABORT_RETRIES; r++) {
            if (kalshi_live_cancel_order(kl, ask_oid[j])) {
                ask_oid[j][0] = '\0';
                break;
            }
            if (r == ABORT_RETRIES - 1)
                fprintf(stderr, "[kalshi] abort: cancel ask order %s failed after %d retries\n", ask_oid[j], ABORT_RETRIES);
        }
    }
    printf("[kalshi] modify failed: mass cancel done, sending abort to Poly\n");
    arb_msg_init(&out, MSG_KALSHI_ABORT);
    arb_ipc_send(fd_out, &out);
    /* fall through */
cascade_failed:
    if (db) arb_db_close(db);
    kalshi_live_destroy(kl);
    printf("[kalshi] live process exiting\n");
}

void kalshi_process_run(int fd_in, int fd_out)
{
    if (!getenv("ARB_LIVE")) {
        fprintf(stderr, "[kalshi] requires Kalshi creds and config (ARB_LIVE not set)\n");
        return;
    }
    kalshi_run_live(fd_in, fd_out);
}


