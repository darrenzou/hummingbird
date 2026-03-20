#define _POSIX_C_SOURCE 200809L
#include "arb_poly.h"
#include "arb_config.h"
#include "poly_live.h"
#include "arb_db.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static void msleep(unsigned int ms) {
    struct timespec ts = { (time_t)(ms / 1000), (long)((ms % 1000) * 1000000) };
    nanosleep(&ts, NULL);
}

static void poly_presign_init(PolyPresignPool *pool)
{
    pool->n_slots = POLY_MAX_BITS;
    for (uint8_t i = 0; i < POLY_MAX_BITS; ++i) {
        pool->slots[i].size = (uint32_t)1u << i;
        pool->slots[i].copies = 5; /* 5 copies per binary slot */
    }
    pool->last_trade_ms = 0;
}

void poly_init_context(PolyContext *ctx)
{
    if (!ctx) return;
    memset(ctx, 0, sizeof(*ctx));
    poly_presign_init(&ctx->presign_pool);
}

void poly_update_book_from_full(const PolyFullBookPayload *msg, PolyContext *ctx)
{
    if (!msg || !ctx) return;

    ctx->state.n_bids = msg->n_bids;
    ctx->state.n_asks = msg->n_asks;
    for (uint16_t i = 0; i < msg->n_bids && i < ARB_MAX_LEVELS; ++i) {
        ctx->state.bids[i].price = msg->bids[i].price;
        ctx->state.bids[i].size  = msg->bids[i].size;
    }
    for (uint16_t i = 0; i < msg->n_asks && i < ARB_MAX_LEVELS; ++i) {
        ctx->state.asks[i].price = msg->asks[i].price;
        ctx->state.asks[i].size  = msg->asks[i].size;
    }

    ctx->state.best_bid = (ctx->state.n_bids > 0) ? ctx->state.bids[0].price : 0.0;
    ctx->state.best_ask = (ctx->state.n_asks > 0) ? ctx->state.asks[0].price : 1.0;
}

static double poly_volume_above(const PolyState *st, int price_cents)
{
    if (!st) return 0.0;
    double threshold = price_cents / 100.0;
    double vol = 0.0;
    for (uint16_t i = 0; i < st->n_bids; ++i) {
        if (st->bids[i].price > threshold) {
            vol += st->bids[i].size;
        }
    }
    return vol;
}

static double poly_volume_below(const PolyState *st, int price_cents)
{
    if (!st) return 0.0;
    double threshold = price_cents / 100.0;
    double vol = 0.0;
    for (uint16_t i = 0; i < st->n_asks; ++i) {
        if (st->asks[i].price < threshold) {
            vol += st->asks[i].size;
        }
    }
    return vol;
}

void poly_setup_tracked_levels(PolyContext *ctx, const KalshiLevelsDonePayload *levels)
{
    if (!ctx || !levels) return;

    ctx->tracker_bid.n_levels = 0;
    ctx->tracker_ask.n_levels = 0;

    /* Bid side levels */
    for (uint8_t i = 0; i < levels->n_bid_levels && i < POLY_MAX_TRACKED; ++i) {
        PolyTrackedLevel *t = &ctx->tracker_bid.levels[ctx->tracker_bid.n_levels++];
        t->in_use = 1;
        t->side = SIDE_BID;
        t->price_cents = levels->bid_price_cents[i];
        t->last_sent_vol = poly_volume_above(&ctx->state, t->price_cents);
    }

    /* Ask side levels */
    for (uint8_t i = 0; i < levels->n_ask_levels && i < POLY_MAX_TRACKED; ++i) {
        PolyTrackedLevel *t = &ctx->tracker_ask.levels[ctx->tracker_ask.n_levels++];
        t->in_use = 1;
        t->side = SIDE_ASK;
        t->price_cents = levels->ask_price_cents[i];
        t->last_sent_vol = poly_volume_below(&ctx->state, t->price_cents);
    }
}

int poly_compute_level_vol_update(PolyContext *ctx, Side side, PolyLevelVolUpdatePayload *out)
{
    if (!ctx || !out) return 0;

    PolyLevelTracker *tracker = (side == SIDE_BID) ? &ctx->tracker_bid : &ctx->tracker_ask;
    if (tracker->n_levels == 0) return 0;

    uint8_t n = 0;
    for (uint8_t i = 0; i < tracker->n_levels && n < ARB_MAX_TRACKED; ++i) {
        PolyTrackedLevel *t = &tracker->levels[i];
        if (!t->in_use) continue;

        double current = (side == SIDE_BID)
                             ? poly_volume_above(&ctx->state, t->price_cents)
                             : poly_volume_below(&ctx->state, t->price_cents);

        double last = t->last_sent_vol;
        int should_send = 0;
        if (last > 0.0) {
            double delta = current - last;
            double frac = delta / last;
            if (frac < 0.0 && -frac > 0.10) {
                should_send = 1; /* decrease >10% */
            } else if (frac > 0.15) {
                should_send = 1; /* increase >15% */
            }
        } else if (current > 0.0) {
            /* If we previously had 0 and now have positive volume, send an update. */
            should_send = 1;
        }

        if (should_send) {
            out->price_cents[n] = t->price_cents;
            out->volume[n] = current;
            t->last_sent_vol = current;
            ++n;
        }
    }

    if (n == 0) {
        return 0;
    }

    out->side = (uint8_t)side;
    out->n_levels = n;
    return 1;
}

static void poly_presign_consume(PolyPresignPool *pool, uint32_t count, uint64_t now)
{
    if (!pool || count == 0) return;

    /* Use binary decomposition: largest powers-of-two first. */
    for (int bit = (int)POLY_MAX_BITS - 1; bit >= 0; --bit) {
        uint32_t slot_size = (uint32_t)1u << bit;
        while (count >= slot_size) {
            PolyPresignSlot *slot = &pool->slots[bit];
            if (slot->copies == 0) {
                /* In a real implementation we would ensure re-sign before this happens. */
                printf("[poly] WARNING: presign slot size=%u exhausted before resign\n",
                       slot_size);
            } else {
                slot->copies--;
                printf("[poly] consume presigned order size=%u, remaining copies=%u\n",
                       slot_size, slot->copies);
                pool->last_trade_ms = now;
                if (slot->copies == 0) {
                    /* Immediate resign rule: refill to 5 copies. */
                    printf("[poly] immediate resign for slot size=%u\n", slot_size);
                    slot->copies = 5;
                }
            }
            count -= slot_size;
        }
    }
}

void poly_handle_kalshi_fill(PolyContext *ctx, const KalshiFillPayload *fill, uint64_t now)
{
    if (!ctx || !fill) return;

    printf("[poly] hedge Kalshi fill: side=%s price_cents=%d filled_count=%u\n",
           (fill->side == SIDE_BID) ? "BID" : "ASK",
           fill->price_cents,
           fill->filled_count);

    poly_presign_consume(&ctx->presign_pool, fill->filled_count, now);
}

void poly_presign_maybe_resign(PolyContext *ctx, uint64_t now)
{
    if (!ctx) return;
    PolyPresignPool *pool = &ctx->presign_pool;
    if (pool->last_trade_ms == 0) return;

    if (now < pool->last_trade_ms) return;
    uint64_t delta = now - pool->last_trade_ms;
    if (delta < 2000) {
        return; /* not yet time for debounced resign */
    }

    int need_resign = 0;
    for (uint8_t i = 0; i < pool->n_slots; ++i) {
        if (pool->slots[i].copies < 5) {
            need_resign = 1;
            break;
        }
    }
    if (!need_resign) return;

    printf("[poly] debounced batch resign after %llu ms\n",
           (unsigned long long)delta);

    for (uint8_t i = 0; i < pool->n_slots; ++i) {
        if (pool->slots[i].copies < 5) {
            pool->slots[i].copies = 5;
        }
    }
    /* Do not reset last_trade_ms; we want this to fire once after trade bursts. */
}

/*
 * Place one order with retry logic. Returns 0 on success, 1 on abort.
 */
static int poly_place_one_with_retry(PolyLive *live, const char *body, uint32_t slot_size,
                                     int is_sell, char *abort_reason, size_t abort_reason_size)
{
    const char *kind = is_sell ? "sell" : "hedge";
    for (;;) {
        printf("[poly] sending %s order size=%u\n", kind, slot_size);
        int r = poly_live_place_order_attempt(live, body);
        if (r == POLY_PLACE_OK) {
            printf("[poly] placed %s order size=%u\n", kind, slot_size);
            return 0;
        }
        if (r == POLY_PLACE_ERR_AUTH) {
            fprintf(stderr, "[poly] %s order auth error -> abort\n", kind);
            if (abort_reason && abort_reason_size) {
                strncpy(abort_reason, "hedge_auth", abort_reason_size - 1);
                abort_reason[abort_reason_size - 1] = '\0';
            }
            return 1;
        }
        if (r == POLY_PLACE_ERR_RATE) {
            fprintf(stderr, "[poly] rate limit, retrying in 0.5s\n");
            msleep(500);
            r = poly_live_place_order_attempt(live, body);
            if (r == POLY_PLACE_OK) {
                printf("[poly] placed %s order size=%u (after rate retry)\n", kind, slot_size);
                return 0;
            }
            fprintf(stderr, "[poly] rate limit retry failed -> abort\n");
            if (abort_reason && abort_reason_size) {
                strncpy(abort_reason, "hedge_rate_limit", abort_reason_size - 1);
                abort_reason[abort_reason_size - 1] = '\0';
            }
            return 1;
        }
        if (r == POLY_PLACE_ERR_NETWORK) {
            int retries = 5;
            while (retries > 0) {
                fprintf(stderr, "[poly] network error, retrying (%d left)\n", retries);
                msleep(200);
                r = poly_live_place_order_attempt(live, body);
                if (r == POLY_PLACE_OK) {
                    printf("[poly] placed %s order size=%u (after network retry)\n", kind, slot_size);
                    return 0;
                }
                if (r != POLY_PLACE_ERR_NETWORK) break;
                retries--;
            }
            if (r == POLY_PLACE_OK) return 0;
            if (r == POLY_PLACE_ERR_NETWORK) {
                fprintf(stderr, "[poly] network error after 5 retries -> abort\n");
                if (abort_reason && abort_reason_size) {
                    strncpy(abort_reason, "hedge_network", abort_reason_size - 1);
                    abort_reason[abort_reason_size - 1] = '\0';
                }
                return 1;
            }
        }
        if (r == POLY_PLACE_ERR_OTHER) {
            fprintf(stderr, "[poly] %s order error (other) -> abort\n", kind);
            if (abort_reason && abort_reason_size) {
                strncpy(abort_reason, "hedge_other", abort_reason_size - 1);
                abort_reason[abort_reason_size - 1] = '\0';
            }
            return 1;
        }
        if (abort_reason && abort_reason_size) abort_reason[0] = '\0';
        return 1;
    }
}

/*
 * Place real hedge orders on Polymarket for a given filled_count (binary decomposition).
 * fill_side: SIDE_BID = Kalshi bought YES -> we buy NO. SIDE_ASK = Kalshi sold YES -> we buy YES.
 * If ARB_NO_TOKEN_ID is set, checks position in opposite token first: sell that before buying.
 * (e.g. need to buy 15 NO, hold 12 YES -> sell 12 YES then buy 3 NO)
 * Returns 0 on success, 1 if caller should send MSG_ABORT and exit.
 */
static int poly_place_hedge_orders(PolyLive *live, int fd_out, uint32_t filled_count,
                                   uint8_t fill_side, char *abort_reason, size_t abort_reason_size)
{
    if (!live || filled_count == 0) return 0;

    const char *token_id = getenv("ARB_TOKEN_ID");      /* YES token */
    const char *no_token_id = getenv("ARB_NO_TOKEN_ID"); /* NO token */

    /* buy_token = what we need to buy; sell_token = what we might sell first */
    const char *buy_token;
    const char *sell_token;
    if (fill_side == SIDE_BID) {
        if (!no_token_id || !no_token_id[0]) {
            fprintf(stderr, "[poly] SIDE_BID hedge requires polymarket_no_token_id in config\n");
            if (abort_reason && abort_reason_size) {
                strncpy(abort_reason, "hedge_no_token_missing", abort_reason_size - 1);
                abort_reason[abort_reason_size - 1] = '\0';
            }
            return 1;
        }
        buy_token = no_token_id;  /* buy NO */
        sell_token = token_id;    /* sell YES first if we have it */
    } else {
        buy_token = token_id;     /* buy YES */
        sell_token = (no_token_id && no_token_id[0]) ? no_token_id : NULL;  /* sell NO first if we have it */
    }

    uint32_t sell_count = 0;
    uint32_t buy_count = filled_count;

    if (sell_token && sell_token[0]) {
        double pos = poly_live_get_position(live, sell_token);
        uint32_t pos_whole = (uint32_t)(pos > 0.0 ? pos : 0);
        if (pos_whole > 0) {
            sell_count = (pos_whole < filled_count) ? pos_whole : filled_count;
            buy_count = filled_count - sell_count;
            printf("[poly] sell-first: position=%.0f in opposite token, sell %u then buy %u\n",
                   pos, sell_count, buy_count);
        }
    }

    char body[4096];

    /* Place sell orders first (opposite token) */
    for (int bit = (int)POLY_MAX_BITS - 1; bit >= 0 && sell_count > 0; --bit) {
        uint32_t slot_size = (uint32_t)1u << bit;
        while (sell_count >= slot_size) {
            if (!poly_live_build_signed_sell_order(live, sell_token, 0.01, (uint64_t)slot_size, body, sizeof(body))) {
                if (abort_reason && abort_reason_size) {
                    strncpy(abort_reason, "hedge_build_sell_failed", abort_reason_size - 1);
                    abort_reason[abort_reason_size - 1] = '\0';
                }
                return 1;
            }
            if (poly_place_one_with_retry(live, body, slot_size, 1, abort_reason, abort_reason_size) != 0)
                return 1;
            sell_count -= slot_size;
        }
    }

    /* Place buy orders */
    for (int bit = (int)POLY_MAX_BITS - 1; bit >= 0; --bit) {
        uint32_t slot_size = (uint32_t)1u << bit;
        while (buy_count >= slot_size) {
            if (!poly_live_build_signed_buy_order(live, buy_token, 0.99, (uint64_t)slot_size, body, sizeof(body))) {
                if (abort_reason && abort_reason_size) {
                    strncpy(abort_reason, "hedge_build_buy_failed", abort_reason_size - 1);
                    abort_reason[abort_reason_size - 1] = '\0';
                }
                return 1;
            }
            if (poly_place_one_with_retry(live, body, slot_size, 0, abort_reason, abort_reason_size) != 0)
                return 1;
            buy_count -= slot_size;
        }
    }
    return 0;
}

void poly_process_run(int fd_in, int fd_out)
{
    ArbCreds creds;
    arb_load_creds(&creds);
    const char *token_id = getenv("ARB_TOKEN_ID");
    const char *neg_s = getenv("ARB_NEG_RISK");
    int neg_risk = (neg_s && neg_s[0] == '1');

    if (!token_id || !token_id[0] || !creds.poly_address[0]) {
        fprintf(stderr, "[poly] requires ARB_TOKEN_ID and Poly creds in .env\n");
        return;
    }

    PolyLive *live = poly_live_create(&creds, token_id, neg_risk);
    if (!live) {
        fprintf(stderr, "[poly] failed to create Poly live client\n");
        return;
    }

    printf("[poly] connecting to Polymarket WebSocket for token %s\n", token_id);
    if (!poly_live_ws_connect(live)) {
        fprintf(stderr, "[poly] WebSocket failed to get orderbook\n");
        poly_live_destroy(live);
        return;
    }

    PolyContext poly;
    poly_init_context(&poly);
    PolyFullBookPayload poly_book_msg;
    poly_live_ws_copy_orderbook(live, &poly_book_msg);
    poly_update_book_from_full(&poly_book_msg, &poly);

    ArbMsg msg;
    arb_msg_init(&msg, MSG_POLY_FULL_BOOK);
    msg.u.poly_full_book = poly_book_msg;
    if (arb_ipc_send(fd_out, &msg) != 0) {
        fprintf(stderr, "[poly] failed to send full book\n");
        poly_live_destroy(live);
        return;
    }
    printf("[poly] sent full book to Kalshi\n");

    if (arb_ipc_recv(fd_in, &msg) != 0) {
        fprintf(stderr, "[poly] failed to receive from Kalshi\n");
        poly_live_destroy(live);
        return;
    }
    if (msg.type == MSG_KALSHI_ABORT) {
        const char *db_path = getenv("ARB_DB_PATH");
        if (!db_path) db_path = "arb_events.db";
        ArbDb *db = NULL;
        if (arb_db_open(db_path, &db) == 0) {
            arb_db_record_abort(db, "kalshi_cascade_failed");
            arb_db_close(db);
        }
        printf("[poly] abort: Kalshi sent ABORT (cascade failed)\n");
        poly_live_destroy(live);
        return;
    }
    if (msg.type != MSG_KALSHI_LEVELS_DONE) {
        fprintf(stderr, "[poly] unexpected message type %d\n", (int)msg.type);
        poly_live_destroy(live);
        return;
    }
    printf("[poly] received cascade levels from Kalshi\n");
    poly_setup_tracked_levels(&poly, &msg.u.kalshi_levels_done);

    for (;;) {
        poly_live_ws_service(live, 50);
        if (poly_live_ws_done(live)) break;

        poly_live_ws_copy_orderbook(live, &poly_book_msg);
        poly_update_book_from_full(&poly_book_msg, &poly);

        /* Abort if price out of band: record reason, tell Kalshi to cancel all, then exit. */
        if (poly.state.best_bid > 0.95 || poly.state.best_ask < 0.05) {
            const char *db_path = getenv("ARB_DB_PATH");
            if (!db_path) db_path = "arb_events.db";
            ArbDb *db = NULL;
            if (arb_db_open(db_path, &db) == 0) {
                arb_db_record_abort(db, "price_out_of_band");
                arb_db_close(db);
            }
            ArbMsg abort_msg;
            arb_msg_init(&abort_msg, MSG_ABORT);
            arb_ipc_send(fd_out, &abort_msg);
            printf("[poly] abort: price out of band (bid=%.2f ask=%.2f)\n",
                   poly.state.best_bid, poly.state.best_ask);
            break;
        }

        PolyLevelVolUpdatePayload upd;
        if (poly_compute_level_vol_update(&poly, SIDE_BID, &upd)) {
            ArbMsg out;
            arb_msg_init(&out, MSG_POLY_LEVEL_VOL_UPDATE);
            out.u.poly_level_vol = upd;
            arb_ipc_send(fd_out, &out);
        }
        if (poly_compute_level_vol_update(&poly, SIDE_ASK, &upd)) {
            ArbMsg out;
            arb_msg_init(&out, MSG_POLY_LEVEL_VOL_UPDATE);
            out.u.poly_level_vol = upd;
            arb_ipc_send(fd_out, &out);
        }

        if (arb_ipc_poll(fd_in, 0) > 0 && arb_ipc_recv(fd_in, &msg) == 0) {
            if (msg.type == MSG_KALSHI_ABORT) {
                const char *db_path = getenv("ARB_DB_PATH");
                if (!db_path) db_path = "arb_events.db";
                ArbDb *db = NULL;
                if (arb_db_open(db_path, &db) == 0) {
                    arb_db_record_abort(db, "kalshi_cascade_failed");
                    arb_db_close(db);
                }
                printf("[poly] abort: Kalshi sent ABORT\n");
                break;
            }
            if (msg.type == MSG_KALSHI_FILL) {
                char abort_reason[64] = { 0 };
                if (poly_place_hedge_orders(live, fd_out, msg.u.kalshi_fill.filled_count,
                                            msg.u.kalshi_fill.side, abort_reason, sizeof(abort_reason)) != 0) {
                    const char *db_path = getenv("ARB_DB_PATH");
                    if (!db_path) db_path = "arb_events.db";
                    ArbDb *db = NULL;
                    if (arb_db_open(db_path, &db) == 0) {
                        arb_db_record_abort(db, abort_reason[0] ? abort_reason : "hedge_error");
                        arb_db_close(db);
                    }
                    ArbMsg abort_msg;
                    arb_msg_init(&abort_msg, MSG_ABORT);
                    arb_ipc_send(fd_out, &abort_msg);
                    printf("[poly] abort: hedge order error (%s)\n", abort_reason[0] ? abort_reason : "unknown");
                    break;
                }
                uint64_t now = (uint64_t)arb_now_ms();
                poly_handle_kalshi_fill(&poly, &msg.u.kalshi_fill, now);
                poly_presign_maybe_resign(&poly, now);
            }
        }
    }

    poly_live_destroy(live);
    printf("[poly] process exiting\n");
}


