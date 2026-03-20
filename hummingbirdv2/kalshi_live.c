/*
 * Kalshi live: REST (place/cancel/amend/balance) + WebSocket (orderbook, user_fills).
 * Order placement is REST-only; WebSocket used for orderbook and fill notifications.
 */
#define _POSIX_C_SOURCE 200809L
#include "kalshi_live.h"
#include "arb_config.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/bio.h>
#include <openssl/buffer.h>
#include <openssl/rsa.h>
#include <curl/curl.h>
#include <libwebsockets.h>
#include <cjson/cJSON.h>

#define REST_BASE "https://api.elections.kalshi.com/trade-api/v2"
#define REST_PFX  "/trade-api/v2"
#define WS_HOST   "api.elections.kalshi.com"
#define WS_PATH   "/trade-api/ws/v2"
#define WS_PORT   443
#define SEQ_OB    1
#define SEQ_FILLS 2
#define K_MAX_LVL 128

typedef struct { int price; int qty; } KLevel;

struct KalshiLive {
    char api_key_id[256];
    char pem_buf[8192];
    char ticker[128];
    EVP_PKEY *pkey;
    CURL *curl;

    /* WS state */
    struct lws_context *ctx;
    struct lws *wsi;
    int got_snapshot;
    int done;
    int subscribe_fills_pending;
    int fills_subscribed;
    char ws_ts[32];
    char ws_sig[1024];

    KLevel yes[K_MAX_LVL];
    int n_yes;
    KLevel no[K_MAX_LVL];
    int n_no;

    int fill_pending;
    double fill_count;
    int fill_is_bid;
    char fill_order_id[64];
};

static KalshiLive *g_cur = NULL;

static char *b64enc(const unsigned char *d, size_t len) {
    BIO *b64 = BIO_new(BIO_f_base64());
    BIO *mem = BIO_new(BIO_s_mem());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    BIO_push(b64, mem);
    BIO_write(b64, d, (int)len);
    BIO_flush(b64);
    BUF_MEM *bm;
    BIO_get_mem_ptr(mem, &bm);
    char *out = malloc(bm->length + 1);
    memcpy(out, bm->data, bm->length);
    out[bm->length] = '\0';
    BIO_free_all(b64);
    return out;
}

static char *rsa_sign(EVP_PKEY *pkey, const char *ts, const char *method, const char *path) {
    char msg[1024];
    snprintf(msg, sizeof(msg), "%s%s%s", ts, method, path);
    EVP_MD_CTX *ctx = EVP_MD_CTX_new();
    EVP_PKEY_CTX *pc = NULL;
    unsigned char *sig = NULL;
    size_t siglen = 0;
    char *out = NULL;
    if (EVP_DigestSignInit(ctx, &pc, EVP_sha256(), NULL, pkey) <= 0) goto done;
    if (EVP_PKEY_CTX_set_rsa_padding(pc, RSA_PKCS1_PSS_PADDING) <= 0) goto done;
    if (EVP_PKEY_CTX_set_rsa_pss_saltlen(pc, -1) <= 0) goto done;
    if (EVP_DigestSign(ctx, NULL, &siglen, (const unsigned char *)msg, strlen(msg)) <= 0) goto done;
    sig = malloc(siglen);
    if (!sig) goto done;
    if (EVP_DigestSign(ctx, sig, &siglen, (const unsigned char *)msg, strlen(msg)) <= 0) goto done;
    out = b64enc(sig, siglen);
done:
    free(sig);
    EVP_MD_CTX_free(ctx);
    return out ? out : strdup("");
}

static struct curl_slist *auth_headers(KalshiLive *k, const char *method, const char *path) {
    char clean[512];
    strncpy(clean, path, sizeof(clean)-1);
    clean[sizeof(clean)-1] = '\0';
    char *q = strchr(clean, '?');
    if (q) *q = '\0';
    char sign_path[640];
    snprintf(sign_path, sizeof(sign_path), "%s%s", REST_PFX, clean);
    char ts[32];
    snprintf(ts, sizeof(ts), "%lld", (long long)arb_now_ms());
    char *sig = rsa_sign(k->pkey, ts, method, sign_path);
    char h1[320], h2[80], h3[1024];
    snprintf(h1, sizeof(h1), "KALSHI-ACCESS-KEY: %s", k->api_key_id);
    snprintf(h2, sizeof(h2), "KALSHI-ACCESS-TIMESTAMP: %s", ts);
    snprintf(h3, sizeof(h3), "KALSHI-ACCESS-SIGNATURE: %s", sig);
    free(sig);
    struct curl_slist *sl = NULL;
    sl = curl_slist_append(sl, h1);
    sl = curl_slist_append(sl, h2);
    sl = curl_slist_append(sl, h3);
    sl = curl_slist_append(sl, "Content-Type: application/json");
    sl = curl_slist_append(sl, "Accept: application/json");
    return sl;
}

typedef struct { char *buf; size_t len; } KResp;
static size_t write_cb(void *data, size_t sz, size_t nmemb, void *u) {
    KResp *r = (KResp *)u;
    size_t n = sz * nmemb;
    r->buf = realloc(r->buf, r->len + n + 1);
    memcpy(r->buf + r->len, data, n);
    r->len += n;
    r->buf[r->len] = '\0';
    return n;
}

/* Optional: pass non-NULL http_status_out to get response code (0 if no response). */
static char *k_http_ex(KalshiLive *k, const char *method, const char *path, const char *body, long *http_status_out) {
    char url[512];
    snprintf(url, sizeof(url), "%s%s", REST_BASE, path);
    struct curl_slist *hdrs = auth_headers(k, method, path);
    KResp r = { NULL, 0 };
    curl_easy_setopt(k->curl, CURLOPT_URL, url);
    curl_easy_setopt(k->curl, CURLOPT_HTTPHEADER, hdrs);
    curl_easy_setopt(k->curl, CURLOPT_WRITEFUNCTION, write_cb);
    curl_easy_setopt(k->curl, CURLOPT_WRITEDATA, &r);
    curl_easy_setopt(k->curl, CURLOPT_FOLLOWLOCATION, 1L);
    if (strcmp(method, "POST") == 0) {
        curl_easy_setopt(k->curl, CURLOPT_POST, 1L);
        curl_easy_setopt(k->curl, CURLOPT_POSTFIELDS, body ? body : "{}");
    } else if (strcmp(method, "DELETE") == 0) {
        curl_easy_setopt(k->curl, CURLOPT_CUSTOMREQUEST, "DELETE");
    } else {
        curl_easy_setopt(k->curl, CURLOPT_HTTPGET, 1L);
    }
    CURLcode cres = curl_easy_perform(k->curl);
    if (cres != CURLE_OK) {
        fprintf(stderr, "[kalshi] HTTP %s %s curl error: %s\n",
                method, path, curl_easy_strerror(cres));
    }
    if (http_status_out) {
        long code = 0;
        curl_easy_getinfo(k->curl, CURLINFO_RESPONSE_CODE, &code);
        *http_status_out = code;
        if (code >= 400) {
            fprintf(stderr, "[kalshi] HTTP %s %s status %ld\n", method, path, code);
        }
    }
    curl_slist_free_all(hdrs);
    curl_easy_setopt(k->curl, CURLOPT_POST, 0L);
    curl_easy_setopt(k->curl, CURLOPT_POSTFIELDS, NULL);
    curl_easy_setopt(k->curl, CURLOPT_CUSTOMREQUEST, NULL);
    return r.buf ? r.buf : strdup("");
}

static char *k_http(KalshiLive *k, const char *method, const char *path, const char *body) {
    return k_http_ex(k, method, path, body, NULL);
}

static void kob_update(KLevel *arr, int *cnt, int price, int delta) {
    for (int i = 0; i < *cnt; i++) {
        if (arr[i].price == price) {
            arr[i].qty += delta;
            if (arr[i].qty <= 0) {
                memmove(&arr[i], &arr[i+1], (size_t)(*cnt - i - 1) * sizeof(KLevel));
                (*cnt)--;
            }
            return;
        }
    }
    if (delta <= 0 || *cnt >= K_MAX_LVL) return;
    arr[*cnt].price = price;
    arr[*cnt].qty = delta;
    (*cnt)++;
    for (int i = *cnt - 1; i > 0 && arr[i].price < arr[i-1].price; i--) {
        KLevel t = arr[i]; arr[i] = arr[i-1]; arr[i-1] = t;
    }
}

static void kob_from_snapshot(KLevel *arr, int *cnt, cJSON *levels) {
    *cnt = 0;
    if (!cJSON_IsArray(levels)) return;
    int n = cJSON_GetArraySize(levels);
    for (int i = 0; i < n; i++) {
        cJSON *l = cJSON_GetArrayItem(levels, i);
        if (!cJSON_IsArray(l) || cJSON_GetArraySize(l) < 2) continue;
        int price = (int)cJSON_GetArrayItem(l, 0)->valuedouble;
        int qty   = (int)cJSON_GetArrayItem(l, 1)->valuedouble;
        if (qty > 0) kob_update(arr, cnt, price, qty);
    }
}

static int ws_cb(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in, size_t len) {
    KalshiLive *k = g_cur;
    if (!k) return 0;
    (void)user;

    switch (reason) {
    case LWS_CALLBACK_CLIENT_APPEND_HANDSHAKE_HEADER: {
        unsigned char **p = (unsigned char **)in, *end = *p + len;
        if (lws_add_http_header_by_name(wsi, (unsigned char *)"KALSHI-ACCESS-KEY:", (unsigned char *)k->api_key_id, (int)strlen(k->api_key_id), p, end) ||
            lws_add_http_header_by_name(wsi, (unsigned char *)"KALSHI-ACCESS-TIMESTAMP:", (unsigned char *)k->ws_ts, (int)strlen(k->ws_ts), p, end) ||
            lws_add_http_header_by_name(wsi, (unsigned char *)"KALSHI-ACCESS-SIGNATURE:", (unsigned char *)k->ws_sig, (int)strlen(k->ws_sig), p, end))
            return -1;
        break;
    }
    case LWS_CALLBACK_CLIENT_ESTABLISHED:
        k->wsi = wsi;
        {
            cJSON *sub = cJSON_CreateObject();
            cJSON_AddNumberToObject(sub, "id", SEQ_OB);
            cJSON_AddStringToObject(sub, "cmd", "subscribe");
            cJSON *params = cJSON_AddObjectToObject(sub, "params");
            cJSON *ch = cJSON_AddArrayToObject(params, "channels");
            cJSON_AddItemToArray(ch, cJSON_CreateString("orderbook_delta"));
            cJSON_AddStringToObject(params, "market_ticker", k->ticker);
            char *msg = cJSON_PrintUnformatted(sub);
            cJSON_Delete(sub);
            size_t mlen = strlen(msg);
            uint8_t *buf = malloc(LWS_PRE + mlen);
            memcpy(buf + LWS_PRE, msg, mlen);
            lws_write(wsi, buf + LWS_PRE, mlen, LWS_WRITE_TEXT);
            free(buf);
            free(msg);
        }
        break;
    case LWS_CALLBACK_CLIENT_WRITEABLE:
        if (k->subscribe_fills_pending && !k->fills_subscribed) {
            k->subscribe_fills_pending = 0;
            k->fills_subscribed = 1;
            cJSON *sub = cJSON_CreateObject();
            cJSON_AddNumberToObject(sub, "id", SEQ_FILLS);
            cJSON_AddStringToObject(sub, "cmd", "subscribe");
            cJSON *params = cJSON_AddObjectToObject(sub, "params");
            cJSON *ch = cJSON_AddArrayToObject(params, "channels");
            cJSON_AddItemToArray(ch, cJSON_CreateString("user_fills"));
            cJSON *tickers = cJSON_AddArrayToObject(params, "market_tickers");
            cJSON_AddItemToArray(tickers, cJSON_CreateString(k->ticker));
            char *msg = cJSON_PrintUnformatted(sub);
            cJSON_Delete(sub);
            size_t mlen = strlen(msg);
            uint8_t *buf = malloc(LWS_PRE + mlen);
            memcpy(buf + LWS_PRE, msg, mlen);
            lws_write(wsi, buf + LWS_PRE, mlen, LWS_WRITE_TEXT);
            free(buf);
            free(msg);
        }
        break;
    case LWS_CALLBACK_CLIENT_RECEIVE: {
        cJSON *root = cJSON_ParseWithLength((char *)in, len);
        if (!root) {
            fprintf(stderr, "[kalshi] WS parse error (len=%zu)\n", len);
            break;
        }
        const char *type = cJSON_GetStringValue(cJSON_GetObjectItem(root, "type"));
        if (!type) { cJSON_Delete(root); break; }
        if (strcmp(type, "orderbook_snapshot") == 0) {
            cJSON *msg = cJSON_GetObjectItem(root, "msg");
            if (!msg) msg = root;
            kob_from_snapshot(k->yes, &k->n_yes, cJSON_GetObjectItem(msg, "yes"));
            kob_from_snapshot(k->no, &k->n_no, cJSON_GetObjectItem(msg, "no"));
            k->got_snapshot = 1;
            printf("[kalshi] orderbook snapshot: yes_levels=%d no_levels=%d\n",
                   k->n_yes, k->n_no);
        } else if (strcmp(type, "orderbook_delta") == 0) {
            cJSON *msg = cJSON_GetObjectItem(root, "msg");
            if (!msg) msg = root;
            int price = (int)(cJSON_GetObjectItem(msg, "price") ? cJSON_GetObjectItem(msg, "price")->valuedouble : 0);
            int delta = (int)(cJSON_GetObjectItem(msg, "delta") ? cJSON_GetObjectItem(msg, "delta")->valuedouble : 0);
            const char *side = cJSON_GetStringValue(cJSON_GetObjectItem(msg, "side"));
            if (side) {
                if (strcmp(side, "yes") == 0) {
                    kob_update(k->yes, &k->n_yes, price, delta);
                    printf("[kalshi] delta: side=yes price=%d delta=%d n_yes=%d\n",
                           price, delta, k->n_yes);
                } else if (strcmp(side, "no") == 0) {
                    kob_update(k->no, &k->n_no, price, delta);
                    printf("[kalshi] delta: side=no price=%d delta=%d n_no=%d\n",
                           price, delta, k->n_no);
                }
            }
        } else if (strcmp(type, "fill") == 0) {
            cJSON *msg = cJSON_GetObjectItem(root, "msg");
            if (!msg) msg = root;
            k->fill_count = cJSON_GetObjectItem(msg, "count") ? cJSON_GetObjectItem(msg, "count")->valuedouble : 0;
            const char *action = cJSON_GetStringValue(cJSON_GetObjectItem(msg, "action"));
            k->fill_is_bid = (action && strcmp(action, "buy") == 0) ? 1 : 0;
            const char *oid = cJSON_GetStringValue(cJSON_GetObjectItem(msg, "order_id"));
            if (oid) {
                strncpy(k->fill_order_id, oid, sizeof(k->fill_order_id)-1);
                k->fill_order_id[sizeof(k->fill_order_id)-1] = '\0';
            }
            k->fill_pending = 1;
            printf("[kalshi] user fill notification: order_id=%s count=%.0f\n",
                   k->fill_order_id, k->fill_count);
        }
        cJSON_Delete(root);
        break;
    }
    case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
        k->done = 1;
        break;
    case LWS_CALLBACK_CLIENT_CLOSED:
        k->done = 1;
        break;
    default:
        break;
    }
    return 0;
}

static struct lws_protocols protos[] = {
    { "kalshi", ws_cb, 0, 262144, 0, NULL, 0 },
    LWS_PROTOCOL_LIST_TERM
};

static void gen_uuid(char *out, size_t sz) {
    snprintf(out, sz, "%08x-%04x-4%03x-%04x-%012x",
             (unsigned)rand(), (unsigned)rand()&0xFFFF, (unsigned)rand()&0x0FFF,
             ((unsigned)rand()&0x3FFF)|0x8000, (unsigned)rand());
}

KalshiLive *kalshi_live_create(const ArbCreds *creds, const char *ticker) {
    if (!creds || !ticker) return NULL;
    KalshiLive *k = calloc(1, sizeof(*k));
    if (!k) return NULL;
    size_t ak = sizeof(k->api_key_id) - 1;
    size_t pk = sizeof(k->pem_buf) - 1;
    memcpy(k->api_key_id, creds->kalshi_api_key_id, ak);
    k->api_key_id[ak] = '\0';
    memcpy(k->pem_buf, creds->kalshi_private_key_pem, pk);
    k->pem_buf[pk] = '\0';
    strncpy(k->ticker, ticker, sizeof(k->ticker)-1);
    BIO *bio = BIO_new_mem_buf(k->pem_buf, -1);
    k->pkey = PEM_read_bio_PrivateKey(bio, NULL, NULL, NULL);
    BIO_free(bio);
    if (!k->pkey) { free(k); return NULL; }
    k->curl = curl_easy_init();
    if (!k->curl) { EVP_PKEY_free(k->pkey); free(k); return NULL; }
    return k;
}

void kalshi_live_destroy(KalshiLive *k) {
    if (!k) return;
    if (k->ctx) lws_context_destroy(k->ctx);
    curl_easy_cleanup(k->curl);
    EVP_PKEY_free(k->pkey);
    free(k);
}

/*
 * Possible errors from batch place (kalshi_live_batch_place_orders):
 *
 * Request-level (return -1 or 0 successes):
 * - Network/curl: timeout, connection refused, DNS failure, TLS error.
 * - 401 Unauthorized: invalid or expired API key, bad signature, wrong timestamp.
 * - 403 Forbidden: account restricted, insufficient permissions, IP not allowed.
 * - 404 Not Found: wrong path (e.g. API version change).
 * - 429 Too Many Requests: rate limit exceeded (batch + other orders in window).
 * - 400 Bad Request: malformed JSON, missing required field, invalid value type.
 * - 422 Unprocessable: validation (e.g. ticker invalid, market closed, negative count).
 * - 500/502/503: server error.
 *
 * Per-order (order_id_out[i] left empty; response.orders[i] may have error message):
 * - Invalid ticker or market closed.
 * - Invalid yes_price (e.g. out of 1–99, or not on tick grid).
 * - Invalid count (e.g. zero, negative, or above limit).
 * - Insufficient balance for total cost.
 * - Duplicate client_order_id.
 * - Order would self-trade (if post_only or exchange rules).
 * - Market paused or not accepting orders.
 */
int kalshi_live_batch_place_orders(KalshiLive *k, const KalshiBatchOrder *orders, int n_orders, char (*order_id_out)[64], long *http_status_out) {
    if (!k || !orders || n_orders <= 0 || n_orders > KALSHI_BATCH_MAX || !order_id_out)
        return 0;
    for (int i = 0; i < n_orders; i++)
        order_id_out[i][0] = '\0';
    if (http_status_out) *http_status_out = 0;

    cJSON *arr = cJSON_CreateArray();
    for (int i = 0; i < n_orders; i++) {
        cJSON *obj = cJSON_CreateObject();
        cJSON_AddStringToObject(obj, "ticker", k->ticker);
        cJSON_AddStringToObject(obj, "side", "yes");
        cJSON_AddStringToObject(obj, "action", orders[i].action);
        cJSON_AddNumberToObject(obj, "count", orders[i].count);
        cJSON_AddNumberToObject(obj, "yes_price", orders[i].yes_price);
        cJSON_AddStringToObject(obj, "time_in_force", "good_till_canceled");
        if (orders[i].client_order_id[0])
            cJSON_AddStringToObject(obj, "client_order_id", orders[i].client_order_id);
        else {
            char coid[64];
            gen_uuid(coid, sizeof(coid));
            cJSON_AddStringToObject(obj, "client_order_id", coid);
        }
        cJSON_AddItemToArray(arr, obj);
    }
    cJSON *body = cJSON_CreateObject();
    cJSON_AddItemToObject(body, "orders", arr);
    char *js = cJSON_PrintUnformatted(body);
    cJSON_Delete(body);
    if (!js) return 0;

    long status = 0;
    char *resp = k_http_ex(k, "POST", "/portfolio/orders/batched", js, &status);
    free(js);
    if (http_status_out) *http_status_out = status;
    if (!resp) return -1;

    cJSON *root = cJSON_Parse(resp);
    free(resp);
    if (!root) return -1;

    int n_ok = 0;
    cJSON *resp_orders = cJSON_GetObjectItem(root, "orders");
    if (cJSON_IsArray(resp_orders)) {
        int sz = cJSON_GetArraySize(resp_orders);
        for (int i = 0; i < sz && i < n_orders; i++) {
            cJSON *item = cJSON_GetArrayItem(resp_orders, i);
            const char *oid = NULL;
            if (cJSON_IsObject(item))
                oid = cJSON_GetStringValue(cJSON_GetObjectItem(item, "order_id"));
            if (oid && oid[0]) {
                strncpy(order_id_out[i], oid, 63);
                order_id_out[i][63] = '\0';
                n_ok++;
            }
        }
    }
    cJSON_Delete(root);
    return n_ok;
}

int kalshi_live_place_order(KalshiLive *k, const char *side, const char *action, int count, int yes_price, char *order_id_out) {
    KalshiBatchOrder one = { .action = action, .count = count, .yes_price = yes_price, .client_order_id = { 0 } };
    char oids[1][64];
    int n = kalshi_live_batch_place_orders(k, &one, 1, oids, NULL);
    if (n == 1 && order_id_out) {
        strncpy(order_id_out, oids[0], 63);
        order_id_out[63] = '\0';
        return 1;
    }
    return 0;
}

int kalshi_live_cancel_order(KalshiLive *k, const char *order_id) {
    if (!order_id || !*order_id) return 0;
    char path[128];
    snprintf(path, sizeof(path), "/portfolio/orders/%s", order_id);
    char *resp = k_http(k, "DELETE", path, NULL);
    if (resp) { free(resp); return 1; }
    return 0;
}

int kalshi_live_amend_order(KalshiLive *k, const char *order_id, const char *side, const char *action, int yes_price, int new_count) {
    if (!order_id || !*order_id) return 0;
    cJSON *body = cJSON_CreateObject();
    cJSON_AddStringToObject(body, "ticker", k->ticker);
    cJSON_AddStringToObject(body, "side", side);
    cJSON_AddStringToObject(body, "action", action);
    cJSON_AddNumberToObject(body, "yes_price", yes_price);
    cJSON_AddNumberToObject(body, "count", new_count);
    char *js = cJSON_PrintUnformatted(body);
    cJSON_Delete(body);
    char path[128];
    snprintf(path, sizeof(path), "/portfolio/orders/%s/amend", order_id);
    char *resp = k_http(k, "POST", path, js);
    free(js);
    int ok = 0;
    if (resp) {
        cJSON *root = cJSON_Parse(resp);
        free(resp);
        if (root) {
            cJSON *order = cJSON_GetObjectItem(root, "order");
            if (!order) order = root;
            const char *status = cJSON_GetStringValue(cJSON_GetObjectItem(order, "status"));
            ok = (status && strcmp(status, "canceled") != 0);
            cJSON_Delete(root);
        }
    }
    return ok;
}

double kalshi_live_get_balance(KalshiLive *k) {
    char *resp = k_http(k, "GET", "/portfolio/balance", NULL);
    double bal = 0.0;
    if (resp) {
        cJSON *root = cJSON_Parse(resp);
        free(resp);
        if (root) {
            cJSON *b = cJSON_GetObjectItem(root, "balance");
            if (b) {
                cJSON *av = cJSON_GetObjectItem(b, "available_balance");
                if (av) bal = av->valuedouble / 100.0;
            }
            cJSON_Delete(root);
        }
    }
    return bal;
}

int kalshi_live_ws_connect(KalshiLive *k) {
    g_cur = k;
    snprintf(k->ws_ts, sizeof(k->ws_ts), "%lld", (long long)arb_now_ms());
    char sign_path[256];
    snprintf(sign_path, sizeof(sign_path), "%s%s", REST_PFX, WS_PATH);
    char *sig = rsa_sign(k->pkey, k->ws_ts, "GET", sign_path);
    strncpy(k->ws_sig, sig, sizeof(k->ws_sig)-1);
    free(sig);

    struct lws_context_creation_info ci = { 0 };
    ci.port = CONTEXT_PORT_NO_LISTEN;
    ci.protocols = protos;
    ci.options = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT | LWS_SERVER_OPTION_DISABLE_IPV6;
    ci.ssl_ca_filepath = "/etc/pki/tls/certs/ca-bundle.crt";
    lws_set_log_level(LLL_ERR | LLL_WARN, NULL);
    k->ctx = lws_create_context(&ci);
    if (!k->ctx) return 0;
    struct lws_client_connect_info cc = { 0 };
    cc.context = k->ctx;
    cc.address = WS_HOST;
    cc.port = WS_PORT;
    cc.path = WS_PATH;
    cc.host = WS_HOST;
    cc.origin = WS_HOST;
    cc.ssl_connection = LCCSCF_USE_SSL;
    cc.protocol = protos[0].name;
    if (!lws_client_connect_via_info(&cc)) {
        lws_context_destroy(k->ctx);
        k->ctx = NULL;
        return 0;
    }
    int64_t deadline = arb_now_ms() + 15000;
    while (!k->got_snapshot && !k->done && arb_now_ms() < deadline) {
        lws_service(k->ctx, 50);
    }
    return k->got_snapshot ? 1 : 0;
}

void kalshi_live_ws_service(KalshiLive *k, int timeout_ms) {
    if (k->ctx) lws_service(k->ctx, timeout_ms);
}

int kalshi_live_ws_got_orderbook(KalshiLive *k) {
    return k->got_snapshot;
}

/* YES bid = yes[] (asc price); YES ask = 100 - no[] (no is NO bid, so NO bid at P => YES ask at 100-P). */
void kalshi_live_ws_copy_orderbook(KalshiLive *k, double *yes_bids, double *bid_sizes, int *n_bids, double *yes_asks, double *ask_sizes, int *n_asks, int max_lvl) {
    int nb = k->n_yes < max_lvl ? k->n_yes : max_lvl;
    int na = k->n_no < max_lvl ? k->n_no : max_lvl;
    *n_bids = nb;
    *n_asks = na;
    for (int i = 0; i < nb; i++) {
        yes_bids[i] = (double)k->yes[i].price;
        bid_sizes[i] = (double)k->yes[i].qty;
    }
    for (int i = 0; i < na; i++) {
        yes_asks[i] = (double)(100 - k->no[i].price);
        ask_sizes[i] = (double)k->no[i].qty;
    }
}

int kalshi_live_ws_done(KalshiLive *k) {
    return k->done;
}

void kalshi_live_ws_subscribe_fills(KalshiLive *k) {
    k->subscribe_fills_pending = 1;
    if (k->wsi) lws_callback_on_writable(k->wsi);
}

int kalshi_live_ws_poll_fill(KalshiLive *k, uint32_t *fill_count_out, int *is_bid_out, char *order_id_out, int order_id_size) {
    if (!k->fill_pending) return 0;
    k->fill_pending = 0;
    if (fill_count_out) *fill_count_out = (uint32_t)k->fill_count;
    if (is_bid_out) *is_bid_out = k->fill_is_bid;
    if (order_id_out && order_id_size > 0) {
        strncpy(order_id_out, k->fill_order_id, (size_t)(order_id_size - 1));
        order_id_out[order_id_size - 1] = '\0';
    }
    return 1;
}
