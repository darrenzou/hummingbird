/*
 * arb_kalshi.c
 *
 * Kalshi process for the cross-exchange arbitrage strategy.
 *
 * Flow:
 *   1. Connect WebSocket, subscribe to orderbook_delta.
 *   2. Wait for poly_book from Poly process AND initial orderbook snapshot.
 *   3. Arb check: poly_bid > kalshi_bid (bid leg) / poly_ask < kalshi_ask (ask leg).
 *      - Both fail: send kalshi_signal{bid_ok=0, ask_ok=0}, abort.
 *      - Either OK: send kalshi_signal with which legs are live.
 *   4. Wait for poly_signing_done.
 *   5. Check if any orderbook_delta changed the best bid/ask since step 3.
 *      - Changed: send redo_signing, go to step 4.
 *      - Stable:  place two Kalshi limit orders.
 *   6. Subscribe to user_fills.
 *   7. Loop: service WS + poll pipe.
 *      - On fill: relay MSG_KALSHI_FILL to Poly.
 *      - On MSG_ABORT (from Poly) or price threshold: cancel all, exit.
 *
 * Crypto dependencies (inlined from kalshi_test.c):
 *   RSA-PSS SHA-256 signing for Kalshi REST authentication.
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_kalshi.h"
#include "arb_ipc.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <sys/select.h>

#include <curl/curl.h>
#include <libwebsockets.h>
#include <cjson/cJSON.h>

#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/bio.h>
#include <openssl/buffer.h>
#include <mysql/mysql.h>
#include <math.h>
#include <signal.h>

/* ── Clean-shutdown signal handling ──────────────────────────────────────── */
static volatile sig_atomic_t g_kalshi_quit = 0;
static void kalshi_quit_handler(int sig) { (void)sig; g_kalshi_quit = 1; }

/* ── Kalshi endpoints ─────────────────────────────────────────────────────── */

#define REST_BASE   "https://api.elections.kalshi.com/trade-api/v2"
#define REST_PFX    "/trade-api/v2"
#define WS_HOST     "api.elections.kalshi.com"
#define WS_PATH     "/trade-api/ws/v2"
#define WS_PORT     443

/* Abort threshold */
#define ABORT_HIGH  95
#define ABORT_LOW    5

/* MySQL/MariaDB RDS connection — override via environment variables:
 *   DB_HOST  DB_PORT  DB_USER  DB_PASS  DB_NAME  DB_SSL_CA
 * Defaults match the RDS instance already provisioned for this project. */
#define DB_HOST_DEFAULT  "database-1.cz6uu00gwfxx.eu-west-1.rds.amazonaws.com"
#define DB_PORT_DEFAULT  3306
#define DB_USER_DEFAULT  "admin"
#define DB_PASS_DEFAULT  "Superstar22!"
#define DB_NAME_DEFAULT  "arb"
#define DB_SSL_CA_DEFAULT "/certs/global-bundle.pem"

/* WS sequence IDs */
#define SEQ_ORDERBOOK 1
#define SEQ_FILLS     2

/* ── Credentials (set by kalshi_run) ─────────────────────────────────────── */

static const char *g_api_key_id;
static char        g_pem_buf[8192];   /* key cached in memory */
static MYSQL      *g_db = NULL;        /* MySQL/RDS orderbook snapshot DB */

/* ── RSA-PSS signing ──────────────────────────────────────────────────────── */

static EVP_PKEY *load_pkey(const char *pem)
{
    BIO *bio = BIO_new_mem_buf(pem, -1);
    EVP_PKEY *k = PEM_read_bio_PrivateKey(bio, NULL, NULL, NULL);
    BIO_free(bio);
    return k;
}

/* Base64-encode bytes (no newlines). Caller frees result. */
static char *b64_encode_k(const unsigned char *data, size_t len)
{
    BIO *b64 = BIO_new(BIO_f_base64());
    BIO *mem = BIO_new(BIO_s_mem());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    BIO_push(b64, mem);
    BIO_write(b64, data, (int)len);
    BIO_flush(b64);
    BUF_MEM *bm;
    BIO_get_mem_ptr(mem, &bm);
    char *out = malloc(bm->length + 1);
    memcpy(out, bm->data, bm->length);
    out[bm->length] = '\0';
    BIO_free_all(b64);
    return out;
}

/*
 * RSA-PSS SHA-256 signature of "{timestamp}{METHOD}{path}" (no query string).
 * Returns heap-allocated base64 string; caller frees.
 */
static char *rsa_sign(EVP_PKEY *pkey, const char *ts,
                      const char *method, const char *path_no_query)
{
    char msg[1024];
    snprintf(msg, sizeof(msg), "%s%s%s", ts, method, path_no_query);

    EVP_MD_CTX *ctx  = EVP_MD_CTX_new();
    EVP_PKEY_CTX *pc = NULL;
    unsigned char *sig  = NULL;
    size_t         siglen = 0;
    char          *out    = NULL;

    if (EVP_DigestSignInit(ctx, &pc, EVP_sha256(), NULL, pkey) <= 0) goto done;
    if (EVP_PKEY_CTX_set_rsa_padding(pc, RSA_PKCS1_PSS_PADDING)  <= 0) goto done;
    if (EVP_PKEY_CTX_set_rsa_pss_saltlen(pc, -1)                  <= 0) goto done;
    if (EVP_DigestSign(ctx, NULL, &siglen,
                       (const unsigned char *)msg, strlen(msg))   <= 0) goto done;
    sig = malloc(siglen);
    if (EVP_DigestSign(ctx, sig, &siglen,
                       (const unsigned char *)msg, strlen(msg))   <= 0) goto done;
    out = b64_encode_k(sig, siglen);
done:
    free(sig);
    EVP_MD_CTX_free(ctx);
    return out ? out : strdup("");
}

/* Build Kalshi authentication headers for a REST request. */
static struct curl_slist *kalshi_auth_headers(EVP_PKEY *pkey,
                                              const char *method,
                                              const char *path)
{
    char ts[32];
    snprintf(ts, sizeof(ts), "%lld", (long long)now_ms());

    /* Strip query params before signing */
    char clean[512];
    strncpy(clean, path, sizeof(clean) - 1);
    char *q = strchr(clean, '?');
    if (q) *q = '\0';

    char sign_path[640];
    snprintf(sign_path, sizeof(sign_path), "%s%s", REST_PFX, clean);

    char *sig = rsa_sign(pkey, ts, method, sign_path);

    char h_key[320], h_ts[80], h_sig[1024];
    snprintf(h_key, sizeof(h_key), "KALSHI-ACCESS-KEY: %s",       g_api_key_id);
    snprintf(h_ts,  sizeof(h_ts),  "KALSHI-ACCESS-TIMESTAMP: %s", ts);
    snprintf(h_sig, sizeof(h_sig), "KALSHI-ACCESS-SIGNATURE: %s", sig);
    free(sig);

    struct curl_slist *sl = NULL;
    sl = curl_slist_append(sl, h_key);
    sl = curl_slist_append(sl, h_ts);
    sl = curl_slist_append(sl, h_sig);
    sl = curl_slist_append(sl, "Content-Type: application/json");
    sl = curl_slist_append(sl, "Accept: application/json");
    return sl;
}

/* ── HTTP helper ──────────────────────────────────────────────────────────── */

typedef struct { char *buf; size_t len; } KResp;

static size_t k_write_cb(void *data, size_t sz, size_t nmemb, void *userp) {
    KResp *r = (KResp *)userp;
    size_t total = sz * nmemb;
    r->buf = realloc(r->buf, r->len + total + 1);
    memcpy(r->buf + r->len, data, total);
    r->len += total;
    r->buf[r->len] = '\0';
    return total;
}

/* Returns heap-allocated body; caller frees. NULL on hard curl error. */
static char *k_http(CURL *curl, EVP_PKEY *pkey,
                    const char *method, const char *path,
                    const char *body_json)
{
    char url[512];
    snprintf(url, sizeof(url), "%s%s", REST_BASE, path);

    struct curl_slist *hdrs = kalshi_auth_headers(pkey, method, path);

    KResp resp = {NULL, 0};
    curl_easy_setopt(curl, CURLOPT_URL,           url);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER,    hdrs);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION,  k_write_cb);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA,      &resp);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);

    if (strcmp(method, "POST") == 0) {
        curl_easy_setopt(curl, CURLOPT_POST,       1L);
        curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body_json ? body_json : "{}");
    } else if (strcmp(method, "DELETE") == 0) {
        curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "DELETE");
    } else {
        curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
    }

    curl_easy_perform(curl);
    curl_slist_free_all(hdrs);
    curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, NULL);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS,    NULL);
    curl_easy_setopt(curl, CURLOPT_POST,          0L);
    return resp.buf ? resp.buf : strdup("");
}

/* ── Kalshi Orderbook ─────────────────────────────────────────────────────── */

#define KALSHI_MAX_LEVELS 128

typedef struct { int price; int qty; } KLevel;

typedef struct {
    KLevel yes[KALSHI_MAX_LEVELS]; int n_yes;  /* YES bids, sorted asc */
    KLevel no [KALSHI_MAX_LEVELS]; int n_no;   /* NO  bids, sorted asc */
} KOrderbook;

/* Insert or update a level; remove if delta makes qty <= 0. */
static void kob_update(KLevel *arr, int *cnt, int price, int delta)
{
    for (int i = 0; i < *cnt; i++) {
        if (arr[i].price == price) {
            arr[i].qty += delta;
            if (arr[i].qty <= 0) {
                memmove(&arr[i], &arr[i+1],
                        (size_t)(*cnt - i - 1) * sizeof(KLevel));
                (*cnt)--;
            }
            return;
        }
    }
    if (delta <= 0 || *cnt >= KALSHI_MAX_LEVELS) return;
    arr[*cnt].price = price;
    arr[*cnt].qty   = delta;
    (*cnt)++;
    /* keep sorted ascending by price */
    for (int i = *cnt - 1; i > 0 && arr[i].price < arr[i-1].price; i--) {
        KLevel tmp = arr[i]; arr[i] = arr[i-1]; arr[i-1] = tmp;
    }
}

/* Populate YES/NO sides from a snapshot array [[price, qty], ...].
 * Delegates to kob_update() so the sorted-ascending invariant is always
 * maintained, regardless of the order the API happens to return levels in. */
static void kob_from_snapshot(KLevel *arr, int *cnt, cJSON *levels_arr)
{
    *cnt = 0;
    if (!cJSON_IsArray(levels_arr)) return;
    int n = cJSON_GetArraySize(levels_arr);
    for (int i = 0; i < n; i++) {
        cJSON *lvl = cJSON_GetArrayItem(levels_arr, i);
        if (!cJSON_IsArray(lvl) || cJSON_GetArraySize(lvl) < 2) continue;
        int price = (int)cJSON_GetArrayItem(lvl, 0)->valuedouble;
        int qty   = (int)cJSON_GetArrayItem(lvl, 1)->valuedouble;
        if (qty > 0)
            kob_update(arr, cnt, price, qty);
    }
}

/* Best YES bid = highest price in yes[] (last element since sorted asc) */
static int kob_best_yes_bid(const KOrderbook *ob)
{
    return ob->n_yes > 0 ? ob->yes[ob->n_yes - 1].price : 0;
}

/* Best YES ask = 100 - best NO bid */
static int kob_best_yes_ask(const KOrderbook *ob)
{
    return ob->n_no > 0 ? 100 - ob->no[ob->n_no - 1].price : 100;
}

/* ── WebSocket state ──────────────────────────────────────────────────────── */

typedef struct {
    int           got_snapshot;
    int           done;

    KOrderbook    ob;

    /* Saved prices at the time we sent kalshi_signal, for delta detection */
    int           sent_bid;
    int           sent_ask;
    int           prices_changed;  /* set when delta changes best bid/ask */

    /* Auth tokens for WS handshake (pre-computed before connect) */
    char          ws_ts [32];
    char          ws_sig[1024];

    /* Subscription state */
    int           subscribe_fills_pending;  /* set to trigger fills subscribe */
    int           fills_subscribed;

    /* Fill notification (written by WS callback, read by main loop) */
    int           fill_pending;
    double        fill_count;
    int           fill_is_bid;
    char          fill_order_id[64];

    /* Ticker */
    char          ticker[128];

    /* Known order IDs so we can match fills and cancel */
    char          bid_order_id[64];
    char          ask_order_id[64];

    /* Live order state for opposite-side adjustment on fills */
    int           bid_remaining;   /* unfilled count remaining on bid order */
    int           ask_remaining;   /* unfilled count remaining on ask order */
    int           bid_price;       /* yes_price (cents) of current bid order */
    int           ask_price;       /* yes_price (cents) of current ask order */

    /* Extra orders placed in response to Poly price updates */
    char          extra_order_ids[32][64];
    int           n_extra_orders;

    struct lws   *wsi;
} KalshiWsCtx;

static KalshiWsCtx g_kws;

/* ── WebSocket callback ───────────────────────────────────────────────────── */

static int kalshi_ws_cb(struct lws *wsi, enum lws_callback_reasons reason,
                        void *user, void *in, size_t len)
{
    (void)user;

    switch (reason) {

    /* Append auth headers to the HTTP Upgrade handshake */
    case LWS_CALLBACK_CLIENT_APPEND_HANDSHAKE_HEADER: {
        unsigned char **p   = (unsigned char **)in;
        unsigned char  *end = *p + len;
        if (lws_add_http_header_by_name(wsi,
                (unsigned char *)"KALSHI-ACCESS-KEY:",
                (unsigned char *)g_api_key_id, (int)strlen(g_api_key_id), p, end))
            return -1;
        if (lws_add_http_header_by_name(wsi,
                (unsigned char *)"KALSHI-ACCESS-TIMESTAMP:",
                (unsigned char *)g_kws.ws_ts, (int)strlen(g_kws.ws_ts), p, end))
            return -1;
        if (lws_add_http_header_by_name(wsi,
                (unsigned char *)"KALSHI-ACCESS-SIGNATURE:",
                (unsigned char *)g_kws.ws_sig, (int)strlen(g_kws.ws_sig), p, end))
            return -1;
        break;
    }

    case LWS_CALLBACK_CLIENT_ESTABLISHED: {
        g_kws.wsi = wsi;
        /* Subscribe to orderbook_delta */
        cJSON *sub  = cJSON_CreateObject();
        cJSON_AddNumberToObject(sub, "id",  SEQ_ORDERBOOK);
        cJSON_AddStringToObject(sub, "cmd", "subscribe");
        cJSON *params = cJSON_AddObjectToObject(sub, "params");
        cJSON *chans  = cJSON_AddArrayToObject(params, "channels");
        cJSON_AddItemToArray(chans, cJSON_CreateString("orderbook_delta"));
        cJSON_AddStringToObject(params, "market_ticker", g_kws.ticker);
        char *msg = cJSON_PrintUnformatted(sub);
        cJSON_Delete(sub);
        size_t mlen = strlen(msg);
        uint8_t *buf = malloc(LWS_PRE + mlen);
        memcpy(buf + LWS_PRE, msg, mlen);
        lws_write(wsi, buf + LWS_PRE, mlen, LWS_WRITE_TEXT);
        free(buf); free(msg);
        break;
    }

    case LWS_CALLBACK_CLIENT_WRITEABLE: {
        if (g_kws.subscribe_fills_pending && !g_kws.fills_subscribed) {
            g_kws.subscribe_fills_pending = 0;
            g_kws.fills_subscribed = 1;

            cJSON *sub    = cJSON_CreateObject();
            cJSON_AddNumberToObject(sub, "id",  SEQ_FILLS);
            cJSON_AddStringToObject(sub, "cmd", "subscribe");
            cJSON *params = cJSON_AddObjectToObject(sub, "params");
            cJSON *chans  = cJSON_AddArrayToObject(params, "channels");
            cJSON_AddItemToArray(chans, cJSON_CreateString("user_fills"));
            cJSON *tickers = cJSON_AddArrayToObject(params, "market_tickers");
            cJSON_AddItemToArray(tickers, cJSON_CreateString(g_kws.ticker));
            char *msg = cJSON_PrintUnformatted(sub);
            cJSON_Delete(sub);
            size_t mlen = strlen(msg);
            uint8_t *buf = malloc(LWS_PRE + mlen);
            memcpy(buf + LWS_PRE, msg, mlen);
            lws_write(wsi, buf + LWS_PRE, mlen, LWS_WRITE_TEXT);
            free(buf); free(msg);
            printf("[kalshi-ws] subscribed to user_fills\n");
        }
        break;
    }

    case LWS_CALLBACK_CLIENT_RECEIVE: {
        cJSON *root = cJSON_ParseWithLength((char *)in, len);
        if (!root) break;

        const char *type = cJSON_GetStringValue(cJSON_GetObjectItem(root, "type"));
        if (!type) { cJSON_Delete(root); break; }

        if (strcmp(type, "orderbook_snapshot") == 0) {
            cJSON *msg_obj = cJSON_GetObjectItem(root, "msg");
            if (!msg_obj) msg_obj = root;
            kob_from_snapshot(g_kws.ob.yes, &g_kws.ob.n_yes,
                              cJSON_GetObjectItem(msg_obj, "yes"));
            kob_from_snapshot(g_kws.ob.no,  &g_kws.ob.n_no,
                              cJSON_GetObjectItem(msg_obj, "no"));
            g_kws.got_snapshot = 1;
            printf("[kalshi-ws] snapshot: best_yes_bid=%d best_yes_ask=%d\n",
                   kob_best_yes_bid(&g_kws.ob), kob_best_yes_ask(&g_kws.ob));

        } else if (strcmp(type, "orderbook_delta") == 0) {
            cJSON *msg_obj = cJSON_GetObjectItem(root, "msg");
            if (!msg_obj) msg_obj = root;
            int   price = (int)(cJSON_GetObjectItem(msg_obj, "price") ?
                                cJSON_GetObjectItem(msg_obj, "price")->valuedouble : 0);
            int   delta = (int)(cJSON_GetObjectItem(msg_obj, "delta") ?
                                cJSON_GetObjectItem(msg_obj, "delta")->valuedouble : 0);
            const char *side = cJSON_GetStringValue(cJSON_GetObjectItem(msg_obj, "side"));

            if (side) {
                if (strcmp(side, "yes") == 0)
                    kob_update(g_kws.ob.yes, &g_kws.ob.n_yes, price, delta);
                else if (strcmp(side, "no") == 0)
                    kob_update(g_kws.ob.no,  &g_kws.ob.n_no,  price, delta);
            }

            /* Track if prices changed vs what we sent */
            int new_bid = kob_best_yes_bid(&g_kws.ob);
            int new_ask = kob_best_yes_ask(&g_kws.ob);
            if (new_bid != g_kws.sent_bid || new_ask != g_kws.sent_ask)
                g_kws.prices_changed = 1;

        } else if (strcmp(type, "fill") == 0) {
            cJSON *msg_obj = cJSON_GetObjectItem(root, "msg");
            if (!msg_obj) msg_obj = root;

            const char *oid  = cJSON_GetStringValue(cJSON_GetObjectItem(msg_obj, "order_id"));
            double count = cJSON_GetObjectItem(msg_obj, "count") ?
                           cJSON_GetObjectItem(msg_obj, "count")->valuedouble : 0;

            /* Determine which order (bid or ask) was filled */
            int is_bid = 0;
            if (oid) {
                if (strcmp(oid, g_kws.bid_order_id) == 0) is_bid = 1;
                else if (strcmp(oid, g_kws.ask_order_id) == 0) is_bid = 0;
                else {
                    /* unknown order – log and skip */
                    printf("[kalshi-ws] fill for unknown order %s\n", oid);
                    cJSON_Delete(root);
                    break;
                }
            }

            g_kws.fill_count   = count;
            g_kws.fill_is_bid  = is_bid;
            g_kws.fill_pending = 1;
            if (oid) strncpy(g_kws.fill_order_id, oid, sizeof(g_kws.fill_order_id)-1);
            printf("[kalshi-ws] fill: %.0f contracts on %s order\n",
                   count, is_bid ? "bid" : "ask");
        }

        cJSON_Delete(root);
        break;
    }

    case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
        fprintf(stderr, "[kalshi-ws] connection error: %s\n",
                in ? (char *)in : "(unknown)");
        g_kws.done = 1;
        break;

    case LWS_CALLBACK_CLIENT_CLOSED:
        fprintf(stderr, "[kalshi-ws] connection closed\n");
        g_kws.done = 1;
        break;

    default:
        break;
    }
    return 0;
}

static struct lws_protocols kalshi_protocols[] = {
    {"kalshi", kalshi_ws_cb, 0, 256*1024, 0, NULL, 0},
    LWS_PROTOCOL_LIST_TERM
};

/* ── Connect and subscribe ────────────────────────────────────────────────── */

static struct lws_context *kalshi_ws_connect(EVP_PKEY *pkey, const char *ticker)
{
    memset(&g_kws, 0, sizeof(g_kws));
    snprintf(g_kws.ticker, sizeof(g_kws.ticker), "%s", ticker);

    /* Pre-compute auth for the WS upgrade (GET /trade-api/ws/v2) */
    snprintf(g_kws.ws_ts, sizeof(g_kws.ws_ts), "%lld", (long long)now_ms());
    {
        char sign_path[128];
        snprintf(sign_path, sizeof(sign_path), "%s%s", REST_PFX, WS_PATH);
        char *sig = rsa_sign(pkey, g_kws.ws_ts, "GET", sign_path);
        strncpy(g_kws.ws_sig, sig, sizeof(g_kws.ws_sig) - 1);
        free(sig);
    }

    struct lws_context_creation_info ci = {0};
    ci.port      = CONTEXT_PORT_NO_LISTEN;
    ci.protocols = kalshi_protocols;
    ci.options   = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT
                 | LWS_SERVER_OPTION_DISABLE_IPV6;
    ci.ssl_ca_filepath = "/etc/pki/tls/certs/ca-bundle.crt";
    lws_set_log_level(LLL_ERR | LLL_WARN, NULL);

    struct lws_context *ctx = lws_create_context(&ci);
    if (!ctx) { fprintf(stderr, "[kalshi-ws] lws_create_context failed\n"); return NULL; }

    struct lws_client_connect_info cc = {0};
    cc.context        = ctx;
    cc.address        = WS_HOST;
    cc.port           = WS_PORT;
    cc.path           = WS_PATH;
    cc.host           = WS_HOST;
    cc.origin         = WS_HOST;
    cc.ssl_connection = LCCSCF_USE_SSL;
    cc.protocol       = kalshi_protocols[0].name;
    cc.userdata       = NULL;

    if (!lws_client_connect_via_info(&cc)) {
        fprintf(stderr, "[kalshi-ws] connect failed\n");
        lws_context_destroy(ctx);
        return NULL;
    }
    return ctx;
}

/* ── UUID generator (for client_order_id) ────────────────────────────────── */

static void gen_uuid(char *out, size_t sz)
{
    snprintf(out, sz, "%08x-%04x-4%03x-%04x-%012x",
             (unsigned)rand(),
             (unsigned)rand() & 0xFFFF,
             (unsigned)rand() & 0x0FFF,
             ((unsigned)rand() & 0x3FFF) | 0x8000,
             (unsigned)rand());
}

/* ── Place a Kalshi limit order ───────────────────────────────────────────── */

/* Returns 1 on success, writes order_id to order_id_out[64]. */
static int place_kalshi_order(CURL *curl, EVP_PKEY *pkey,
                               const char *ticker, const char *side,
                               const char *action, int count, int yes_price,
                               char *order_id_out)
{
    char coid[64];
    gen_uuid(coid, sizeof(coid));

    cJSON *body = cJSON_CreateObject();
    cJSON_AddStringToObject(body, "ticker",          ticker);
    cJSON_AddStringToObject(body, "side",            side);
    cJSON_AddStringToObject(body, "action",          action);
    cJSON_AddNumberToObject(body, "count",           count);
    cJSON_AddNumberToObject(body, "yes_price",       yes_price);
    cJSON_AddStringToObject(body, "time_in_force",   "good_till_canceled");
    cJSON_AddStringToObject(body, "client_order_id", coid);
    char *body_str = cJSON_PrintUnformatted(body);
    cJSON_Delete(body);

    char *resp = k_http(curl, pkey, "POST", "/portfolio/orders", body_str);
    free(body_str);

    if (!resp) return 0;

    cJSON *pr    = cJSON_Parse(resp);
    free(resp);
    if (!pr) return 0;

    cJSON *order = cJSON_GetObjectItem(pr, "order");
    if (!order) order = pr;
    const char *oid = cJSON_GetStringValue(cJSON_GetObjectItem(order, "order_id"));
    int ok = 0;
    if (oid && order_id_out) {
        strncpy(order_id_out, oid, 63);
        ok = 1;
        printf("[kalshi] placed %s %s order: id=%s price=%d count=%d\n",
               action, side, oid, yes_price, count);
    } else {
        fprintf(stderr, "[kalshi] order placement failed: %.400s\n",
                cJSON_PrintUnformatted(pr));
    }
    cJSON_Delete(pr);
    return ok;
}


/* ── MySQL/RDS orderbook snapshot persistence ────────────────────────────── */

/*
 * Open a connection to the RDS MySQL instance and create the
 * orderbook_snapshots table if it does not already exist.
 *
 * Connection parameters are read from environment variables first;
 * compiled-in defaults are used as fallbacks.
 *
 * Schema:
 *   ts_ms        – milliseconds since Unix epoch
 *   ticker       – Kalshi market ticker
 *   fill_order   – order_id that triggered the snapshot
 *   fill_amount  – contracts filled on this notification
 *   fill_is_bid  – 1 = bid order filled, 0 = ask order filled
 *   yes_levels   – JSON array [[price_cents, qty], …] (all YES bids)
 *   no_levels    – JSON array [[price_cents, qty], …] (all NO bids)
 */
static void db_open(void)
{
    const char *host   = getenv("DB_HOST")   ? getenv("DB_HOST")   : DB_HOST_DEFAULT;
    const char *user   = getenv("DB_USER")   ? getenv("DB_USER")   : DB_USER_DEFAULT;
    const char *pass   = getenv("DB_PASS")   ? getenv("DB_PASS")   : DB_PASS_DEFAULT;
    const char *dbname = getenv("DB_NAME")   ? getenv("DB_NAME")   : DB_NAME_DEFAULT;
    const char *sslca  = getenv("DB_SSL_CA") ? getenv("DB_SSL_CA") : DB_SSL_CA_DEFAULT;
    unsigned int port  = getenv("DB_PORT") ? (unsigned)atoi(getenv("DB_PORT")) : DB_PORT_DEFAULT;

    g_db = mysql_init(NULL);
    if (!g_db) {
        fprintf(stderr, "[db] mysql_init failed\n");
        return;
    }

    /* Enable SSL with the RDS CA bundle (MariaDB client API) */
    mysql_options(g_db, MYSQL_OPT_SSL_CA, sslca);
    my_bool verify = 1;
    mysql_options(g_db, MYSQL_OPT_SSL_VERIFY_SERVER_CERT, &verify);

    /* Reconnect automatically if the connection drops */
    my_bool reconnect = 1;
    mysql_options(g_db, MYSQL_OPT_RECONNECT, &reconnect);

    if (!mysql_real_connect(g_db, host, user, pass, NULL, port, NULL, 0)) {
        fprintf(stderr, "[db] connect failed: %s\n", mysql_error(g_db));
        mysql_close(g_db);
        g_db = NULL;
        return;
    }

    /* Create database if not present, then select it */
    char ddl_db[256];
    snprintf(ddl_db, sizeof(ddl_db),
             "CREATE DATABASE IF NOT EXISTS `%s`", dbname);
    if (mysql_query(g_db, ddl_db)) {
        fprintf(stderr, "[db] CREATE DATABASE: %s\n", mysql_error(g_db));
        mysql_close(g_db); g_db = NULL; return;
    }
    if (mysql_select_db(g_db, dbname)) {
        fprintf(stderr, "[db] USE %s: %s\n", dbname, mysql_error(g_db));
        mysql_close(g_db); g_db = NULL; return;
    }

    const char *ddl_tbl =
        "CREATE TABLE IF NOT EXISTS orderbook_snapshots ("
        "  id          BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,"
        "  ts_ms       BIGINT      NOT NULL,"
        "  ticker      VARCHAR(64) NOT NULL,"
        "  fill_order  VARCHAR(64) NOT NULL,"
        "  fill_amount DOUBLE      NOT NULL,"
        "  fill_is_bid TINYINT     NOT NULL,"
        "  yes_levels  TEXT        NOT NULL,"
        "  no_levels   TEXT        NOT NULL,"
        "  INDEX idx_ticker (ticker),"
        "  INDEX idx_ts    (ts_ms)"
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
    if (mysql_query(g_db, ddl_tbl)) {
        fprintf(stderr, "[db] CREATE TABLE: %s\n", mysql_error(g_db));
    } else {
        printf("[db] Connected to %s@%s:%u/%s\n", user, host, port, dbname);
    }
}

/* Serialise one side of the Kalshi orderbook as a compact JSON array. */
static void ob_side_to_json(const KLevel *arr, int cnt, char *out, size_t sz)
{
    size_t pos = 0;
    pos += (size_t)snprintf(out + pos, sz - pos, "[");
    for (int i = 0; i < cnt && pos < sz - 20; i++) {
        if (i) pos += (size_t)snprintf(out + pos, sz - pos, ",");
        pos += (size_t)snprintf(out + pos, sz - pos,
                                "[%d,%d]", arr[i].price, arr[i].qty);
    }
    snprintf(out + pos, sz - pos, "]");
}

/*
 * Insert one row into orderbook_snapshots capturing the current
 * KOrderbook state at the moment of a fill, using a prepared statement.
 */
static void db_save_orderbook(const KOrderbook *ob,
                              const char *ticker,
                              const char *fill_order_id,
                              double fill_amount, int fill_is_bid)
{
    if (!g_db) return;

    char yes_json[4096], no_json[4096];
    ob_side_to_json(ob->yes, ob->n_yes, yes_json, sizeof(yes_json));
    ob_side_to_json(ob->no,  ob->n_no,  no_json,  sizeof(no_json));

    MYSQL_STMT *stmt = mysql_stmt_init(g_db);
    if (!stmt) {
        fprintf(stderr, "[db] stmt init: %s\n", mysql_error(g_db));
        return;
    }

    const char *sql =
        "INSERT INTO orderbook_snapshots"
        " (ts_ms, ticker, fill_order, fill_amount, fill_is_bid,"
        "  yes_levels, no_levels)"
        " VALUES (?, ?, ?, ?, ?, ?, ?)";

    if (mysql_stmt_prepare(stmt, sql, (unsigned long)strlen(sql))) {
        fprintf(stderr, "[db] prepare: %s\n", mysql_stmt_error(stmt));
        mysql_stmt_close(stmt);
        return;
    }

    long long ts       = (long long)now_ms();
    long long is_bid_i = fill_is_bid;
    unsigned long ticker_len  = (unsigned long)strlen(ticker);
    unsigned long order_len   = (unsigned long)strlen(fill_order_id);
    unsigned long yes_len     = (unsigned long)strlen(yes_json);
    unsigned long no_len      = (unsigned long)strlen(no_json);

    MYSQL_BIND bind[7];
    memset(bind, 0, sizeof(bind));

    bind[0].buffer_type   = MYSQL_TYPE_LONGLONG;
    bind[0].buffer        = &ts;

    bind[1].buffer_type   = MYSQL_TYPE_STRING;
    bind[1].buffer        = (char *)ticker;
    bind[1].buffer_length = ticker_len;
    bind[1].length        = &ticker_len;

    bind[2].buffer_type   = MYSQL_TYPE_STRING;
    bind[2].buffer        = (char *)fill_order_id;
    bind[2].buffer_length = order_len;
    bind[2].length        = &order_len;

    bind[3].buffer_type   = MYSQL_TYPE_DOUBLE;
    bind[3].buffer        = &fill_amount;

    bind[4].buffer_type   = MYSQL_TYPE_LONGLONG;
    bind[4].buffer        = &is_bid_i;

    bind[5].buffer_type   = MYSQL_TYPE_STRING;
    bind[5].buffer        = yes_json;
    bind[5].buffer_length = yes_len;
    bind[5].length        = &yes_len;

    bind[6].buffer_type   = MYSQL_TYPE_STRING;
    bind[6].buffer        = no_json;
    bind[6].buffer_length = no_len;
    bind[6].length        = &no_len;

    if (mysql_stmt_bind_param(stmt, bind) ||
        mysql_stmt_execute(stmt)) {
        fprintf(stderr, "[db] insert: %s\n", mysql_stmt_error(stmt));
    } else {
        printf("[db] snapshot saved (fill=%.0f %s order %s)\n",
               fill_amount, fill_is_bid ? "bid" : "ask", fill_order_id);
    }

    mysql_stmt_close(stmt);
}

/* Fetch available Kalshi cash balance (dollars). 0.0 on error. */
/*
 * GET /portfolio/balance with RSA auth.
 * Kalshi returns available_balance as an integer number of cents.
 */
static double fetch_kalshi_balance(CURL *curl, EVP_PKEY *pkey)
{
    char *resp = k_http(curl, pkey, "GET", "/portfolio/balance", NULL);
    if (!resp) return 0.0;

    double balance = 0.0;
    cJSON *root = cJSON_Parse(resp);
    free(resp);
    if (!root) return 0.0;

    /* { "balance": { "available_balance": <cents int> } } */
    cJSON *bal_obj = cJSON_GetObjectItem(root, "balance");
    if (!bal_obj) bal_obj = root;
    cJSON *avail = cJSON_GetObjectItem(bal_obj, "available_balance");
    if (!avail)   avail = cJSON_GetObjectItem(root, "available_balance");
    if (avail)
        balance = avail->valuedouble / 100.0;   /* cents to dollars */

    cJSON_Delete(root);
    printf("[kalshi] available balance: $%.2f\n", balance);
    return balance;
}

/* ── Cancel a Kalshi order ────────────────────────────────────────────────── */

static void cancel_kalshi_order(CURL *curl, EVP_PKEY *pkey,
                                const char *order_id)
{
    if (!order_id || !*order_id) return;
    char path[128];
    snprintf(path, sizeof(path), "/portfolio/orders/%s", order_id);
    char *resp = k_http(curl, pkey, "DELETE", path, NULL);
    if (resp) { printf("[kalshi] cancel %s: %.100s\n", order_id, resp); free(resp); }
}

/* ── Amend a Kalshi order (change count in-place, POST …/amend) ───────────── */

/*
 * POST /portfolio/orders/{order_id}/amend
 * Sets the order's new desired count without cancelling it.
 * `new_count` is the updated total quantity (new remaining after amendment).
 * Returns 1 on success, 0 on failure.
 */
static int amend_kalshi_order(CURL *curl, EVP_PKEY *pkey,
                              const char *order_id, const char *ticker,
                              const char *side, const char *action,
                              int yes_price, int new_count)
{
    if (!order_id || !*order_id) return 0;

    cJSON *body = cJSON_CreateObject();
    cJSON_AddStringToObject(body, "ticker", ticker);
    cJSON_AddStringToObject(body, "side",   side);
    cJSON_AddStringToObject(body, "action", action);
    cJSON_AddNumberToObject(body, "yes_price", yes_price);
    cJSON_AddNumberToObject(body, "count",     new_count);
    char *body_str = cJSON_PrintUnformatted(body);
    cJSON_Delete(body);

    char path[128];
    snprintf(path, sizeof(path), "/portfolio/orders/%s/amend", order_id);
    char *resp = k_http(curl, pkey, "POST", path, body_str);
    free(body_str);
    if (!resp) return 0;

    cJSON *pr = cJSON_Parse(resp);
    free(resp);
    int ok = 0;
    if (pr) {
        /* Response has old_order and order; check order.status */
        cJSON *order = cJSON_GetObjectItem(pr, "order");
        if (!order) order = pr;
        const char *status = cJSON_GetStringValue(
            cJSON_GetObjectItem(order, "status"));
        ok = (status && strcmp(status, "canceled") != 0);
        printf("[kalshi] amend %s count->%d: status=%s\n",
               order_id, new_count, status ? status : "?");
        cJSON_Delete(pr);
    }
    return ok;
}

/* ── arb spread check helper ─────────────────────────────────────────────────
 * Called whenever either side's best price changes (Poly price message or
 * Kalshi orderbook delta).  Cancels a resting order when the spread has
 * collapsed to <=1c; recreates it when spread is restored above 1c. */
static void check_arb_spread(CURL *curl, EVP_PKEY *pkey,
                              const ArbConfig *cfg,
                              double poly_bid, double poly_ask,
                              int bid_ok, int ask_ok,
                              int bid_count, int ask_count)
{
    int k_bid = kob_best_yes_bid(&g_kws.ob);
    int k_ask = kob_best_yes_ask(&g_kws.ob);
    int poly_bid_c = (int)(poly_bid * 100.0 + 0.5);
    int poly_ask_c = (int)(poly_ask * 100.0 + 0.5);

    if (bid_ok) {
        int spread = poly_bid_c - k_bid;
        if (g_kws.bid_order_id[0] && spread <= 1) {
            printf("[kalshi] Bid arb lost (poly=%dc k=%dc spread=%dc) -- cancelling\n",
                   poly_bid_c, k_bid, spread);
            cancel_kalshi_order(curl, pkey, g_kws.bid_order_id);
            g_kws.bid_order_id[0] = '\0';
            g_kws.bid_remaining   = 0;
        } else if (!g_kws.bid_order_id[0] && spread > 1) {
            printf("[kalshi] Bid arb restored (spread=%dc) -- placing bid order\n", spread);
            place_kalshi_order(curl, pkey, cfg->kalshi_ticker,
                               "yes", "buy", bid_count, k_bid,
                               g_kws.bid_order_id);
            g_kws.bid_price     = k_bid;
            g_kws.bid_remaining = bid_count;
        }
    }
    if (ask_ok) {
        int spread = k_ask - poly_ask_c;
        if (g_kws.ask_order_id[0] && spread <= 1) {
            printf("[kalshi] Ask arb lost (poly=%dc k=%dc spread=%dc) -- cancelling\n",
                   poly_ask_c, k_ask, spread);
            cancel_kalshi_order(curl, pkey, g_kws.ask_order_id);
            g_kws.ask_order_id[0] = '\0';
            g_kws.ask_remaining   = 0;
        } else if (!g_kws.ask_order_id[0] && spread > 1) {
            printf("[kalshi] Ask arb restored (spread=%dc) -- placing ask order\n", spread);
            place_kalshi_order(curl, pkey, cfg->kalshi_ticker,
                               "yes", "sell", ask_count, k_ask,
                               g_kws.ask_order_id);
            g_kws.ask_price     = k_ask;
            g_kws.ask_remaining = ask_count;
        }
    }
}

/* ── kalshi_run: main state machine ──────────────────────────────────────── */

void kalshi_run(const ArbConfig *cfg, const ArbCreds *creds,
                int fd_to_poly, int fd_from_poly)
{
    g_api_key_id = creds->kalshi_api_key_id;
    snprintf(g_pem_buf, sizeof(g_pem_buf), "%s", creds->kalshi_private_key_pem);

    srand((unsigned)time(NULL) ^ (unsigned)getpid() ^ 0xCAFE);

    signal(SIGTERM, kalshi_quit_handler);
    signal(SIGINT,  kalshi_quit_handler);

    /* Open orderbook snapshot database */
    db_open();

    /* Load RSA private key */
    EVP_PKEY *pkey = load_pkey(g_pem_buf);
    if (!pkey) {
        fprintf(stderr, "[kalshi] failed to load RSA private key\n");
        return;
    }

    /* libcurl */
    CURL *curl = curl_easy_init();
    if (!curl) { fprintf(stderr, "[kalshi] curl init failed\n"); goto cleanup_key; }
    curl_easy_setopt(curl, CURLOPT_TCP_NODELAY,    1L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE,  1L);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);

    /* ── Connect WebSocket ── */
    printf("[kalshi] Connecting to Kalshi WS for %s…\n", cfg->kalshi_ticker);
    struct lws_context *ctx = kalshi_ws_connect(pkey, cfg->kalshi_ticker);
    if (!ctx) goto cleanup_curl;

    /* ── Wait for WS snapshot AND poly_book message ── */
    printf("[kalshi] Waiting for orderbook snapshot and poly_book…\n");
    double poly_bid = 0, poly_ask = 0, poly_bid_vol = 0, poly_ask_vol = 0, poly_balance = 0;
    int got_poly_book = 0;
    {
        int64_t deadline = now_ms() + 60000;
        while (now_ms() < deadline && !g_kws.done) {
            lws_service(ctx, 10);

            if (!got_poly_book) {
                ArbMsg msg = {0};
                int rc = ipc_recv_nb(fd_from_poly, &msg);
                if (rc == 1 && msg.type == MSG_POLY_BOOK) {
                    poly_bid     = msg.d.poly_book.bid;
                    poly_ask     = msg.d.poly_book.ask;
                    poly_bid_vol = msg.d.poly_book.bid_vol;
                    poly_ask_vol = msg.d.poly_book.ask_vol;
                    got_poly_book = 1;
                    printf("[kalshi] Got poly_book: bid=%.4f ask=%.4f\n",
                           poly_bid, poly_ask);
                } else if (rc == -1) goto cleanup_ws;
            }

            if (got_poly_book && g_kws.got_snapshot) break;
        }
        if (!got_poly_book || !g_kws.got_snapshot) {
            fprintf(stderr, "[kalshi] timed out waiting for snapshot or poly_book\n");
            goto cleanup_ws;
        }
    }

    /* ── Arb check ── */
    int k_bid = kob_best_yes_bid(&g_kws.ob);
    int k_ask = kob_best_yes_ask(&g_kws.ob);
    printf("[kalshi] Arb check: poly_bid=%.4f(%.0f¢) kalshi_bid=%d¢  "
           "poly_ask=%.4f(%.0f¢) kalshi_ask=%d¢\n",
           poly_bid, poly_bid*100, k_bid,
           poly_ask, poly_ask*100, k_ask);

    int bid_ok = (int)(poly_bid * 100 + 0.5) > k_bid;
    int ask_ok = (int)(poly_ask * 100 + 0.5) < k_ask;
    {
        ArbMsg m = {0};
        m.type                       = MSG_KALSHI_SIGNAL;
        m.d.kalshi_signal.bid_ok     = bid_ok;
        m.d.kalshi_signal.ask_ok     = ask_ok;
        m.d.kalshi_signal.kalshi_bid = k_bid;
        m.d.kalshi_signal.kalshi_ask = k_ask;
        ipc_send(fd_to_poly, &m);
    }

    if (!bid_ok && !ask_ok) {
        printf("[kalshi] No arb opportunity — aborting\n");
        goto cleanup_ws;
    }
    printf("[kalshi] Arb confirmed! bid_leg=%s ask_leg=%s kalshi_bid=%d kalshi_ask=%d\n",
           bid_ok ? "YES" : "no", ask_ok ? "YES" : "no", k_bid, k_ask);

    /* Save prices sent to Poly for delta tracking */
    g_kws.sent_bid      = k_bid;
    g_kws.sent_ask      = k_ask;
    g_kws.prices_changed = 0;

wait_signing_done:
    /* ── Wait for poly_signing_done ── */
    printf("[kalshi] Waiting for poly_signing_done…\n");
    {
        int64_t deadline = now_ms() + 120000;
        int got = 0;
        while (!got && !g_kws.done && now_ms() < deadline) {
            lws_service(ctx, 10);
            ArbMsg msg = {0};
            int rc = ipc_recv_nb(fd_from_poly, &msg);
            if (rc == 1) {
                if (msg.type == MSG_POLY_SIGNING_DONE) {
                    poly_bid_vol  = msg.d.poly_signing_done.poly_bid_vol;
                    poly_ask_vol  = msg.d.poly_signing_done.poly_ask_vol;
                    poly_balance  = msg.d.poly_signing_done.poly_balance;
                    got = 1;
                } else if (msg.type == MSG_ABORT) {
                    printf("[kalshi] Abort from Poly\n");
                    goto cleanup_ws;
                }
            } else if (rc == -1) goto cleanup_ws;
        }
        if (!got) {
            fprintf(stderr, "[kalshi] timed out waiting for signing_done\n");
            goto cleanup_ws;
        }
    }

    /* ── Check for orderbook changes since we sent the arb signal ── */
    {
        lws_service(ctx, 5);  /* drain any pending WS frames */
        int new_bid = kob_best_yes_bid(&g_kws.ob);
        int new_ask = kob_best_yes_ask(&g_kws.ob);

        if (g_kws.prices_changed || new_bid != g_kws.sent_bid || new_ask != g_kws.sent_ask) {
            printf("[kalshi] Prices changed (bid: %d→%d, ask: %d→%d) — requesting redo\n",
                   g_kws.sent_bid, new_bid, g_kws.sent_ask, new_ask);
            g_kws.sent_bid       = new_bid;
            g_kws.sent_ask       = new_ask;
            g_kws.prices_changed = 0;

            ArbMsg m = {0};
            m.type                   = MSG_POLY_REDO_SIGNING;
            m.d.redo_signing.kalshi_bid = new_bid;
            m.d.redo_signing.kalshi_ask = new_ask;
            ipc_send(fd_to_poly, &m);
            goto wait_signing_done;
        }
    }

    /* ── Prices stable — place Kalshi limit orders ── */

    /*
     * Budget-scaling condition:
     * base_bid  = poly_bid_vol  * 0.75
     * base_ask  = poly_ask_vol  * 0.75
     * base_total = base_bid + base_ask
     *
     * If base_total < min(kalshi_balance, poly_balance), we have more
     * cash than the default 75%-of-top-of-book sizing would use.  In
     * that case scale up proportionally so the full budget is deployed:
     *   bid_count = (base_bid  / base_total) * budget
     *   ask_count = (base_ask  / base_total) * budget
     * where budget = min(kalshi_balance, poly_balance).
     */
    double kalshi_balance = fetch_kalshi_balance(curl, pkey);

    double base_bid   = poly_bid_vol * 0.75;
    double base_ask   = poly_ask_vol * 0.75;
    double base_total = base_bid + base_ask;

    int bid_count, ask_count;
    if (base_total > 0 && poly_balance > 0 && kalshi_balance > 0
        && base_total < (kalshi_balance < poly_balance ? kalshi_balance : poly_balance)) {
        double budget = kalshi_balance < poly_balance ? kalshi_balance : poly_balance;
        bid_count = (int)(base_bid / base_total * budget);
        ask_count = (int)(base_ask / base_total * budget);
        printf("[kalshi] Budget scaling active: budget=$%.2f bid_count=%d ask_count=%d\n",
               budget, bid_count, ask_count);
    } else {
        bid_count = (int)base_bid;
        ask_count = (int)base_ask;
        printf("[kalshi] Standard 75%% sizing: bid_count=%d ask_count=%d\n",
               bid_count, ask_count);
    }
    if (bid_count < 1) bid_count = 1;
    if (ask_count < 1) ask_count = 1;

    int bid_price = g_kws.sent_bid;   /* join best bid level */
    int ask_price = g_kws.sent_ask;   /* join best ask level */

    /* Clamp to valid range */
    if (bid_price > 99) bid_price = 99;
    if (ask_price > 99) ask_price = 99;

    if (bid_ok) {
        printf("[kalshi] Placing bid order: count=%d yes_price=%d\n",
               bid_count, bid_price);
        place_kalshi_order(curl, pkey, cfg->kalshi_ticker,
                           "yes", "buy", bid_count, bid_price,
                           g_kws.bid_order_id);
    }

    if (ask_ok) {
        printf("[kalshi] Placing ask order: count=%d yes_price=%d\n",
               ask_count, ask_price);
        place_kalshi_order(curl, pkey, cfg->kalshi_ticker,
                           "yes", "sell", ask_count, ask_price,
                           g_kws.ask_order_id);
    }

    /* Store live order state for opposite-side adjustment */
    g_kws.bid_remaining = bid_ok ? bid_count : 0;
    g_kws.ask_remaining = ask_ok ? ask_count : 0;
    g_kws.bid_price     = bid_price;
    g_kws.ask_price     = ask_price;

    /* ── Subscribe to user_fills ── */
    g_kws.subscribe_fills_pending = 1;
    lws_callback_on_writable(g_kws.wsi);

    /* ── Monitoring loop ── */
    printf("[kalshi] Monitoring loop started\n");
    while (!g_kws.done) {
        lws_service(ctx, 10);

        if (g_kalshi_quit) {
            printf("[kalshi] Shutdown signal — cancelling orders\n");
            if (bid_ok && g_kws.bid_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.bid_order_id);
            if (ask_ok && g_kws.ask_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.ask_order_id);
            goto cleanup_ws;
        }

        /* Check price threshold */
        int cur_bid = kob_best_yes_bid(&g_kws.ob);
        int cur_ask = kob_best_yes_ask(&g_kws.ob);
        if (cur_bid > ABORT_HIGH || cur_bid < ABORT_LOW ||
            cur_ask > ABORT_HIGH || cur_ask < ABORT_LOW) {
            printf("[kalshi] Price threshold breached (bid=%d ask=%d) — aborting\n",
                   cur_bid, cur_ask);
            if (bid_ok && g_kws.bid_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.bid_order_id);
            if (ask_ok && g_kws.ask_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.ask_order_id);
            ArbMsg am = {0};
            am.type           = MSG_ABORT;
            am.d.abort_msg.reason = 1;
            ipc_send(fd_to_poly, &am);
            goto cleanup_ws;
        }


        /* Kalshi orderbook delta: re-evaluate arb spread immediately */
        if (g_kws.prices_changed) {
            g_kws.prices_changed = 0;
            check_arb_spread(curl, pkey, cfg,
                             poly_bid, poly_ask,
                             bid_ok, ask_ok,
                             bid_count, ask_count);
        }

        /* Relay fills to Poly, then adjust the opposite resting order */
        if (g_kws.fill_pending) {
            g_kws.fill_pending = 0;
            int    fill_amt = (int)g_kws.fill_count;
            int    is_bid   = g_kws.fill_is_bid;

            /* Snapshot the orderbook (after latest WS delta) into the DB */
            db_save_orderbook(&g_kws.ob, g_kws.ticker,
                              g_kws.fill_order_id,
                              g_kws.fill_count, is_bid);

            /* Send fill notification to Poly */
            ArbMsg fm = {0};
            fm.type                       = MSG_KALSHI_FILL;
            fm.d.kalshi_fill.filled_count = g_kws.fill_count;
            fm.d.kalshi_fill.is_bid       = is_bid;
            snprintf(fm.d.kalshi_fill.order_id,
                     sizeof(fm.d.kalshi_fill.order_id),
                     "%s", g_kws.fill_order_id);
            ipc_send(fd_to_poly, &fm);

            /*
             * Adjust the OPPOSITE resting order:
             * When the bid order partially fills, add the fill amount
             * to the ask order's volume (cancel + replace), and vice versa.
             */
            if (is_bid) {
                /* Bid filled: reduce bid remaining, grow ask in-place if active */
                g_kws.bid_remaining -= fill_amt;
                if (ask_ok) {
                    int new_ask = g_kws.ask_remaining + fill_amt;
                    printf("[kalshi] Bid fill %d: amending ask %d -> %d\n",
                           fill_amt, g_kws.ask_remaining, new_ask);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.ask_order_id, cfg->kalshi_ticker,
                                       "yes", "sell",
                                       g_kws.ask_price, new_ask);
                    g_kws.ask_remaining = new_ask;
                }
            } else {
                /* Ask filled: reduce ask remaining, grow bid in-place if active */
                g_kws.ask_remaining -= fill_amt;
                if (bid_ok) {
                    int new_bid = g_kws.bid_remaining + fill_amt;
                    printf("[kalshi] Ask fill %d: amending bid %d -> %d\n",
                           fill_amt, g_kws.bid_remaining, new_bid);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.bid_order_id, cfg->kalshi_ticker,
                                       "yes", "buy",
                                       g_kws.bid_price, new_bid);
                    g_kws.bid_remaining = new_bid;
                }
            }
        }

        /* Poll pipe: handle ABORT and Poly price updates */
        ArbMsg pipe_msg = {0};
        int rc = ipc_recv_nb(fd_from_poly, &pipe_msg);
        if (rc == 1) {
            switch (pipe_msg.type) {
            case MSG_ABORT:
                printf("[kalshi] Abort from Poly (reason=%d)\n",
                       pipe_msg.d.abort_msg.reason);
                if (bid_ok && g_kws.bid_order_id[0])
                    cancel_kalshi_order(curl, pkey, g_kws.bid_order_id);
                if (ask_ok && g_kws.ask_order_id[0])
                    cancel_kalshi_order(curl, pkey, g_kws.ask_order_id);
                goto cleanup_ws;
            case MSG_POLY_PRICE_UPDATE: {
                double new_bid = pipe_msg.d.poly_price_update.bid;
                double new_ask = pipe_msg.d.poly_price_update.ask;
                poly_bid = new_bid;   /* keep cached Poly prices current */
                poly_ask = new_ask;
                int    cur_k_bid = kob_best_yes_bid(&g_kws.ob);
                int    cur_k_ask = kob_best_yes_ask(&g_kws.ob);
                int    poly_bid_c = (int)(poly_bid * 100.0 + 0.5);
                int    poly_ask_c = (int)(poly_ask * 100.0 + 0.5);
                printf("[kalshi] Poly price update: bid=%.4f ask=%.4f "
                       "(kalshi bid=%d ask=%d)\n",
                       new_bid, new_ask, cur_k_bid, cur_k_ask);
                /* Step 1: spread check via helper (cancel/recreate) */
                check_arb_spread(curl, pkey, cfg,
                                 poly_bid, poly_ask,
                                 bid_ok, ask_ok,
                                 bid_count, ask_count);
                /* Step 2: reprice — only reached if the order survived step 1.
                 * If Poly moved within 1c of Kalshi and Kalshi's best price has
                 * shifted since the order was placed, amend to track it. */
                if (bid_ok && g_kws.bid_order_id[0] &&
                    fabs(new_bid - cur_k_bid / 100.0) <= 0.01 &&
                    cur_k_bid != g_kws.bid_price) {
                    int ep = cur_k_bid < 1 ? 1 : cur_k_bid > 99 ? 99 : cur_k_bid;
                    printf("[kalshi] Repricing bid %d -> %d\n", g_kws.bid_price, ep);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.bid_order_id, cfg->kalshi_ticker,
                                       "yes", "buy", ep, g_kws.bid_remaining);
                    g_kws.bid_price = ep;
                }
                if (ask_ok && g_kws.ask_order_id[0] &&
                    fabs(new_ask - cur_k_ask / 100.0) <= 0.01 &&
                    cur_k_ask != g_kws.ask_price) {
                    int ep = cur_k_ask < 1 ? 1 : cur_k_ask > 99 ? 99 : cur_k_ask;
                    printf("[kalshi] Repricing ask %d -> %d\n", g_kws.ask_price, ep);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.ask_order_id, cfg->kalshi_ticker,
                                       "yes", "sell", ep, g_kws.ask_remaining);
                    g_kws.ask_price = ep;
                }
                break;
            }
            case MSG_POLY_VOL_UPDATE: {
                double new_bid_vol = pipe_msg.d.poly_vol_update.bid_vol;
                double new_ask_vol = pipe_msg.d.poly_vol_update.ask_vol;
                printf("[kalshi] Poly vol update: bid_vol=%.0f ask_vol=%.0f\n",
                       new_bid_vol, new_ask_vol);
                /* Amend resting orders down to 75% of new vol (never increase) */
                int new_bid_sz = (int)(new_bid_vol * 0.75);
                int new_ask_sz = (int)(new_ask_vol * 0.75);
                if (new_bid_sz < 1) new_bid_sz = 1;
                if (new_ask_sz < 1) new_ask_sz = 1;
                if (bid_ok && g_kws.bid_order_id[0] &&
                    new_bid_sz != g_kws.bid_remaining) {
                    printf("[kalshi] Amending bid %d -> %d (vol update)\n",
                           g_kws.bid_remaining, new_bid_sz);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.bid_order_id, cfg->kalshi_ticker,
                                       "yes", "buy",
                                       g_kws.bid_price, new_bid_sz);
                    g_kws.bid_remaining = new_bid_sz;
                }
                if (ask_ok && g_kws.ask_order_id[0] &&
                    new_ask_sz != g_kws.ask_remaining) {
                    printf("[kalshi] Amending ask %d -> %d (vol update)\n",
                           g_kws.ask_remaining, new_ask_sz);
                    amend_kalshi_order(curl, pkey,
                                       g_kws.ask_order_id, cfg->kalshi_ticker,
                                       "yes", "sell",
                                       g_kws.ask_price, new_ask_sz);
                    g_kws.ask_remaining = new_ask_sz;
                }
                break;
            }
            default:
                break;
            }
        } else if (rc == -1) {
            fprintf(stderr, "[kalshi] pipe EOF\n");
            if (bid_ok && g_kws.bid_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.bid_order_id);
            if (ask_ok && g_kws.ask_order_id[0])
                cancel_kalshi_order(curl, pkey, g_kws.ask_order_id);
            goto cleanup_ws;
        }
    }

cleanup_ws:
    /* Cancel any extra orders placed on Poly price updates */
    {
        int i;
        for (i = 0; i < g_kws.n_extra_orders; i++)
            cancel_kalshi_order(curl, pkey,
                                g_kws.extra_order_ids[i]);
    }
    lws_context_destroy(ctx);
cleanup_curl:
    curl_easy_cleanup(curl);
cleanup_key:
    EVP_PKEY_free(pkey);
    if (g_db) { mysql_close(g_db); g_db = NULL; mysql_library_end(); }
    printf("[kalshi] Process exiting\n");
}
