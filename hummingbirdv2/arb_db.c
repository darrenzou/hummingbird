#include "arb_db.h"

#include <sqlite3.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#define ARB_DB_MAX_LEVELS 128

struct arb_db {
    sqlite3 *db;
};

static int run_schema(ArbDb *adb)
{
    const char *sql =
        "CREATE TABLE IF NOT EXISTS events ("
        "  id INTEGER PRIMARY KEY AUTOINCREMENT,"
        "  ts TEXT NOT NULL,"
        "  action_type TEXT NOT NULL,"
        "  fill_price_cents INTEGER,"
        "  fill_amount INTEGER,"
        "  resize_price_cents INTEGER,"
        "  resize_vol_before REAL,"
        "  resize_vol_after REAL,"
        "  resize_side TEXT,"
        "  abort_reason TEXT"
        ");"
        "CREATE TABLE IF NOT EXISTS event_orderbook_levels ("
        "  event_id INTEGER NOT NULL REFERENCES events(id),"
        "  venue TEXT NOT NULL,"
        "  side TEXT NOT NULL,"
        "  level_index INTEGER NOT NULL,"
        "  price REAL NOT NULL,"
        "  size REAL NOT NULL"
        ");"
        "CREATE INDEX IF NOT EXISTS idx_ob_event ON event_orderbook_levels(event_id);";
    char *err = NULL;
    if (sqlite3_exec(adb->db, sql, NULL, NULL, &err) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] schema: %s\n", err ? err : "unknown");
        if (err) sqlite3_free(err);
        return -1;
    }
    (void)sqlite3_exec(adb->db, "ALTER TABLE events ADD COLUMN abort_reason TEXT;", NULL, NULL, NULL);
    return 0;
}

static void timestamp_iso(char *buf, size_t bufsiz)
{
    time_t t = time(NULL);
    struct tm *tm = gmtime(&t);
    if (tm) {
        snprintf(buf, bufsiz, "%04d-%02d-%02dT%02d:%02d:%02dZ",
                 tm->tm_year + 1900, tm->tm_mon + 1, tm->tm_mday,
                 tm->tm_hour, tm->tm_min, tm->tm_sec);
    } else {
        snprintf(buf, bufsiz, "1970-01-01T00:00:00Z");
    }
}

static int insert_orderbook_levels(ArbDb *adb, sqlite3_int64 event_id,
                                   const char *venue,
                                   const ArbDbLevel *bids, uint16_t n_bids,
                                   const ArbDbLevel *asks, uint16_t n_asks)
{
    sqlite3_stmt *stmt = NULL;
    const char *sql = "INSERT INTO event_orderbook_levels (event_id, venue, side, level_index, price, size) VALUES (?, ?, ?, ?, ?, ?);";
    if (sqlite3_prepare_v2(adb->db, sql, -1, &stmt, NULL) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] prepare ob levels: %s\n", sqlite3_errmsg(adb->db));
        return -1;
    }

    for (uint16_t i = 0; i < n_bids && i < ARB_DB_MAX_LEVELS; i++) {
        sqlite3_bind_int64(stmt, 1, event_id);
        sqlite3_bind_text(stmt, 2, venue, -1, SQLITE_STATIC);
        sqlite3_bind_text(stmt, 3, "bid", -1, SQLITE_STATIC);
        sqlite3_bind_int(stmt, 4, (int)i);
        sqlite3_bind_double(stmt, 5, bids[i].price);
        sqlite3_bind_double(stmt, 6, bids[i].size);
        if (sqlite3_step(stmt) != SQLITE_DONE) {
            fprintf(stderr, "[arb_db] insert ob level: %s\n", sqlite3_errmsg(adb->db));
            sqlite3_finalize(stmt);
            return -1;
        }
        sqlite3_reset(stmt);
    }
    for (uint16_t i = 0; i < n_asks && i < ARB_DB_MAX_LEVELS; i++) {
        sqlite3_bind_int64(stmt, 1, event_id);
        sqlite3_bind_text(stmt, 2, venue, -1, SQLITE_STATIC);
        sqlite3_bind_text(stmt, 3, "ask", -1, SQLITE_STATIC);
        sqlite3_bind_int(stmt, 4, (int)i);
        sqlite3_bind_double(stmt, 5, asks[i].price);
        sqlite3_bind_double(stmt, 6, asks[i].size);
        if (sqlite3_step(stmt) != SQLITE_DONE) {
            fprintf(stderr, "[arb_db] insert ob level: %s\n", sqlite3_errmsg(adb->db));
            sqlite3_finalize(stmt);
            return -1;
        }
        sqlite3_reset(stmt);
    }
    sqlite3_finalize(stmt);
    return 0;
}

int arb_db_open(const char *path, ArbDb **out_db)
{
    if (!path || !out_db) return -1;
    ArbDb *adb = (ArbDb *)malloc(sizeof(*adb));
    if (!adb) return -1;
    adb->db = NULL;

    if (sqlite3_open(path, &adb->db) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] open %s: %s\n", path, sqlite3_errmsg(adb->db));
        if (adb->db) sqlite3_close(adb->db);
        free(adb);
        return -1;
    }
    if (run_schema(adb) != 0) {
        sqlite3_close(adb->db);
        free(adb);
        return -1;
    }
    *out_db = adb;
    return 0;
}

void arb_db_close(ArbDb *db)
{
    if (!db) return;
    if (db->db) sqlite3_close(db->db);
    free(db);
}

int arb_db_record_start(ArbDb *adb,
                        const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                        const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                        const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                        const ArbDbLevel *poly_asks, uint16_t n_poly_asks)
{
    if (!adb || !adb->db) return -1;

    char ts[64];
    timestamp_iso(ts, sizeof(ts));

    sqlite3_stmt *stmt = NULL;
    const char *sql = "INSERT INTO events (ts, action_type) VALUES (?, 'start');";
    if (sqlite3_prepare_v2(adb->db, sql, -1, &stmt, NULL) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] prepare start: %s\n", sqlite3_errmsg(adb->db));
        return -1;
    }
    sqlite3_bind_text(stmt, 1, ts, -1, SQLITE_TRANSIENT);
    if (sqlite3_step(stmt) != SQLITE_DONE) {
        fprintf(stderr, "[arb_db] insert start event: %s\n", sqlite3_errmsg(adb->db));
        sqlite3_finalize(stmt);
        return -1;
    }
    sqlite3_int64 event_id = sqlite3_last_insert_rowid(adb->db);
    sqlite3_finalize(stmt);

    if (insert_orderbook_levels(adb, event_id, "kalshi", kalshi_bids, n_kalshi_bids, kalshi_asks, n_kalshi_asks) != 0)
        return -1;
    if (insert_orderbook_levels(adb, event_id, "poly", poly_bids, n_poly_bids, poly_asks, n_poly_asks) != 0)
        return -1;

    return 0;
}

int arb_db_record_fill(ArbDb *adb,
                       const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                       const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                       const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                       const ArbDbLevel *poly_asks, uint16_t n_poly_asks,
                       int fill_price_cents, uint32_t fill_amount)
{
    if (!adb || !adb->db) return -1;

    char ts[64];
    timestamp_iso(ts, sizeof(ts));

    sqlite3_stmt *stmt = NULL;
    const char *sql = "INSERT INTO events (ts, action_type, fill_price_cents, fill_amount) VALUES (?, 'fill', ?, ?);";
    if (sqlite3_prepare_v2(adb->db, sql, -1, &stmt, NULL) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] prepare fill: %s\n", sqlite3_errmsg(adb->db));
        return -1;
    }
    sqlite3_bind_text(stmt, 1, ts, -1, SQLITE_TRANSIENT);
    sqlite3_bind_int(stmt, 2, fill_price_cents);
    sqlite3_bind_int64(stmt, 3, (sqlite3_int64)fill_amount);
    if (sqlite3_step(stmt) != SQLITE_DONE) {
        fprintf(stderr, "[arb_db] insert fill event: %s\n", sqlite3_errmsg(adb->db));
        sqlite3_finalize(stmt);
        return -1;
    }
    sqlite3_int64 event_id = sqlite3_last_insert_rowid(adb->db);
    sqlite3_finalize(stmt);

    if (insert_orderbook_levels(adb, event_id, "kalshi", kalshi_bids, n_kalshi_bids, kalshi_asks, n_kalshi_asks) != 0)
        return -1;
    if (insert_orderbook_levels(adb, event_id, "poly", poly_bids, n_poly_bids, poly_asks, n_poly_asks) != 0)
        return -1;

    return 0;
}

int arb_db_record_resize(ArbDb *adb,
                        const ArbDbLevel *kalshi_bids, uint16_t n_kalshi_bids,
                        const ArbDbLevel *kalshi_asks, uint16_t n_kalshi_asks,
                        const ArbDbLevel *poly_bids, uint16_t n_poly_bids,
                        const ArbDbLevel *poly_asks, uint16_t n_poly_asks,
                        int price_cents, int is_bid,
                        double vol_before, double vol_after)
{
    if (!adb || !adb->db) return -1;

    char ts[64];
    timestamp_iso(ts, sizeof(ts));

    sqlite3_stmt *stmt = NULL;
    const char *sql = "INSERT INTO events (ts, action_type, resize_price_cents, resize_vol_before, resize_vol_after, resize_side) VALUES (?, 'resize', ?, ?, ?, ?);";
    if (sqlite3_prepare_v2(adb->db, sql, -1, &stmt, NULL) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] prepare resize: %s\n", sqlite3_errmsg(adb->db));
        return -1;
    }
    sqlite3_bind_text(stmt, 1, ts, -1, SQLITE_TRANSIENT);
    sqlite3_bind_int(stmt, 2, price_cents);
    sqlite3_bind_double(stmt, 3, vol_before);
    sqlite3_bind_double(stmt, 4, vol_after);
    sqlite3_bind_text(stmt, 5, is_bid ? "bid" : "ask", -1, SQLITE_STATIC);
    if (sqlite3_step(stmt) != SQLITE_DONE) {
        fprintf(stderr, "[arb_db] insert resize event: %s\n", sqlite3_errmsg(adb->db));
        sqlite3_finalize(stmt);
        return -1;
    }
    sqlite3_int64 event_id = sqlite3_last_insert_rowid(adb->db);
    sqlite3_finalize(stmt);

    if (insert_orderbook_levels(adb, event_id, "kalshi", kalshi_bids, n_kalshi_bids, kalshi_asks, n_kalshi_asks) != 0)
        return -1;
    if (insert_orderbook_levels(adb, event_id, "poly", poly_bids, n_poly_bids, poly_asks, n_poly_asks) != 0)
        return -1;

    return 0;
}

int arb_db_record_abort(ArbDb *adb, const char *reason)
{
    if (!adb || !adb->db) return -1;
    char ts[64];
    timestamp_iso(ts, sizeof(ts));
    sqlite3_stmt *stmt = NULL;
    const char *sql = "INSERT INTO events (ts, action_type, abort_reason) VALUES (?, 'abort', ?);";
    if (sqlite3_prepare_v2(adb->db, sql, -1, &stmt, NULL) != SQLITE_OK) {
        fprintf(stderr, "[arb_db] prepare abort: %s\n", sqlite3_errmsg(adb->db));
        return -1;
    }
    sqlite3_bind_text(stmt, 1, ts, -1, SQLITE_TRANSIENT);
    sqlite3_bind_text(stmt, 2, reason ? reason : "", -1, SQLITE_TRANSIENT);
    if (sqlite3_step(stmt) != SQLITE_DONE) {
        fprintf(stderr, "[arb_db] insert abort event: %s\n", sqlite3_errmsg(adb->db));
        sqlite3_finalize(stmt);
        return -1;
    }
    sqlite3_finalize(stmt);
    return 0;
}
