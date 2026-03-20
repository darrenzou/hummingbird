/*
 * nyc_weather_ws.c
 *
 * C rewrite of nyc_weather_order.py.
 * Uses the Kalshi WebSocket API to fetch the live orderbook snapshot and the
 * REST API to find the market, place a 1-contract limit order, and cancel it.
 * Measures and prints the round-trip latency for each operation.
 *
 * Flow:
 *   1. REST  GET  /markets?series_ticker=KXHIGHNY  → find market ticker
 *   2. WSS   orderbook_delta subscribe              → receive snapshot  (timed)
 *   3. REST  POST /portfolio/orders                 → place order       (timed)
 *   4. REST  DELETE /portfolio/orders/{id}          → cancel order      (timed)
 *   5. Print timing summary
 *
 * Dependencies:
 *   libcurl       – REST HTTP requests
 *   libwebsockets – WebSocket client (v4.x)
 *   cjson         – JSON parsing / building
 *   openssl       – RSA-PSS signing, base64 encoding
 *
 * Build (Linux / macOS):
 *   gcc -O2 -o nyc_weather_ws nyc_weather_ws.c \
 *       -lcurl -lwebsockets -lcjson -lssl -lcrypto -lpthread
 *
 * Build (Windows, MinGW):
 *   gcc -O2 -o nyc_weather_ws.exe nyc_weather_ws.c \
 *       -lcurl -lwebsockets -lcjson -lssl -lcrypto -lpthread -lws2_32
 */

 #include <stdio.h>
 #include <stdlib.h>
 #include <string.h>
 #include <stdint.h>
 #include <time.h>
 
 #ifdef _WIN32
 #  include <winsock2.h>   /* must come before windows.h */
 #  include <windows.h>
    typedef LARGE_INTEGER hr_time_t;
    static void     hr_now(hr_time_t *t) { QueryPerformanceCounter(t); }
    static double   hr_ms(hr_time_t *a, hr_time_t *b) {
        LARGE_INTEGER f; QueryPerformanceFrequency(&f);
        return (double)(b->QuadPart - a->QuadPart) * 1000.0 / f.QuadPart;
    }
    static int64_t  now_ms(void) {
        FILETIME ft; GetSystemTimeAsFileTime(&ft);
        ULARGE_INTEGER u; u.LowPart = ft.dwLowDateTime; u.HighPart = ft.dwHighDateTime;
        return (int64_t)((u.QuadPart - 116444736000000000ULL) / 10000);
    }
 #else
 #  include <unistd.h>
    typedef struct timespec hr_time_t;
    static void     hr_now(hr_time_t *t) { clock_gettime(CLOCK_MONOTONIC, t); }
    static double   hr_ms(hr_time_t *a, hr_time_t *b) {
        return (b->tv_sec - a->tv_sec) * 1000.0 + (b->tv_nsec - a->tv_nsec) / 1e6;
    }
    static int64_t  now_ms(void) {
        struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
        return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
    }
 #endif
 
 #include <curl/curl.h>
 #include <libwebsockets.h>
 #include <cjson/cJSON.h>
 #include <openssl/evp.h>
 #include <openssl/pem.h>
 #include <openssl/rsa.h>
 #include <openssl/bio.h>
 #include <openssl/buffer.h>
 #include <openssl/ssl.h>    /* OPENSSL_init_ssl */
 
/* ── Credentials ─────────────────────────────────────────────────────────── */
/* Read from environment variables (set via: source .env or export in shell)  */

static const char *API_KEY_ID      = NULL;
static const char *PRIVATE_KEY_PEM = NULL;
/* PEM content cached in memory so we can reload the key after lws_context_destroy
 * calls OPENSSL_cleanup(), which makes BIO_new_file unusable. */
static char g_pem_buf[8192] = {0};
 
 /* ── Config ──────────────────────────────────────────────────────────────── */
 
 #define REST_BASE     "https://api.elections.kalshi.com/trade-api/v2"
 #define REST_PFX      "/trade-api/v2"   /* prepended to path when signing */
 #define WS_HOST       "api.elections.kalshi.com"
 #define WS_PATH       "/trade-api/ws/v2"
 #define WS_PORT       443
 #define NYC_SERIES    "KXHIGHNY"
 #define MAX_RESP      (4 * 1024 * 1024)
 #define SIG_SZ        600
 
 /* ── HTTP response buffer ────────────────────────────────────────────────── */
 
 typedef struct { char *data; size_t len, cap; } RespBuf;
 
 static size_t write_cb(char *ptr, size_t sz, size_t n, void *ud)
 {
     RespBuf *b = (RespBuf *)ud;
     size_t inc = sz * n;
     if (b->len + inc + 1 > b->cap) {
         size_t nc = b->cap * 2 + inc + 1;
         if (nc > MAX_RESP) { fprintf(stderr, "[warn] response truncated\n"); return 0; }
         char *tmp = realloc(b->data, nc);
         if (!tmp) return 0;
         b->data = tmp; b->cap = nc;
     }
     memcpy(b->data + b->len, ptr, inc);
     b->len += inc;
     b->data[b->len] = '\0';
     return inc;
 }
 
 static RespBuf *buf_new(void)
 {
     RespBuf *b = malloc(sizeof(RespBuf));
     b->cap = 8192; b->len = 0;
     b->data = malloc(b->cap);
     b->data[0] = '\0';
     return b;
 }
 
 static void buf_free(RespBuf *b) { if (b) { free(b->data); free(b); } }
 
 /* ── Crypto: RSA-PSS SHA-256 → base64 ───────────────────────────────────── */
 
static EVP_PKEY *load_pkey(const char *pem)
{
    BIO *bio = BIO_new_mem_buf(pem, -1);
    EVP_PKEY *k = PEM_read_bio_PrivateKey(bio, NULL, NULL, NULL);
    BIO_free(bio);
    return k;
}

static EVP_PKEY *load_pkey_file(const char *path)
{
    BIO *bio = BIO_new_file(path, "r");
    if (!bio) return NULL;
    EVP_PKEY *k = PEM_read_bio_PrivateKey(bio, NULL, NULL, NULL);
    BIO_free(bio);
    return k;
}
 
 /* Signs `msg` with RSA-PSS / SHA-256, writes base64 into `out`. */
 static int sign_b64(EVP_PKEY *pkey, const char *msg, char *out, size_t outsz)
 {
     EVP_MD_CTX *ctx = EVP_MD_CTX_new();
     EVP_PKEY_CTX *pctx = NULL;
     unsigned char *sig = NULL;
     size_t siglen = 0;
     int ok = 0;
 
     if (EVP_DigestSignInit(ctx, &pctx, EVP_sha256(), NULL, pkey) <= 0) goto done;
     /* salt length == digest length (matches Python padding.PSS.DIGEST_LENGTH) */
     if (EVP_PKEY_CTX_set_rsa_padding(pctx, RSA_PKCS1_PSS_PADDING) <= 0) goto done;
     if (EVP_PKEY_CTX_set_rsa_pss_saltlen(pctx, -1) <= 0) goto done;
     if (EVP_DigestSign(ctx, NULL, &siglen, (unsigned char *)msg, strlen(msg)) <= 0) goto done;
     sig = malloc(siglen);
     if (EVP_DigestSign(ctx, sig, &siglen, (unsigned char *)msg, strlen(msg)) <= 0) goto done;
 
     {   /* base64-encode with no newlines */
         BIO *b64 = BIO_new(BIO_f_base64());
         BIO *mem = BIO_new(BIO_s_mem());
         BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
         BIO_push(b64, mem);
         BIO_write(b64, sig, (int)siglen);
         BIO_flush(b64);
         BUF_MEM *bptr;
         BIO_get_mem_ptr(mem, &bptr);
         if (bptr->length + 1 <= outsz) {
             memcpy(out, bptr->data, bptr->length);
             out[bptr->length] = '\0';
             ok = 1;
         }
         BIO_free_all(b64);
     }
 done:
     free(sig);
     EVP_MD_CTX_free(ctx);
     return ok;
 }
 
 /*
  * Populate `ts_out` (timestamp ms string) and `sig_out` (base64 signature)
  * for the given HTTP method + full API path (including /trade-api/v2 prefix).
  */
 static void make_auth(EVP_PKEY *pkey, const char *method, const char *path,
                       char *ts_out, char *sig_out)
 {
     snprintf(ts_out, 32, "%lld", (long long)now_ms());
     /* strip query params before signing */
     char clean[512];
     strncpy(clean, path, sizeof(clean) - 1);
     char *q = strchr(clean, '?');
     if (q) *q = '\0';
     char msg[1024];
     snprintf(msg, sizeof(msg), "%s%s%s", ts_out, method, clean);
     sign_b64(pkey, msg, sig_out, SIG_SZ);
 }
 
 /* ── REST helper ─────────────────────────────────────────────────────────── */
 
 /*
  * Execute an authenticated HTTP request. `path` is the endpoint relative to
  * REST_BASE, e.g. "/markets?...".  Returns a heap-allocated response body
  * (caller must free) or NULL on error.
  */
 static char *http_req(CURL *curl, EVP_PKEY *pkey,
                       const char *method, const char *path,
                       const char *body_json)
 {
     char url[1024];
     snprintf(url, sizeof(url), "%s%s", REST_BASE, path);
 
     /* Full path for signing: /trade-api/v2 + /endpoint */
     char sign_path[512];
     snprintf(sign_path, sizeof(sign_path), "%s%s", REST_PFX, path);
 
     char ts[32], sig[SIG_SZ];
     make_auth(pkey, method, sign_path, ts, sig);
 
    char h_key[320], h_ts[80], h_sig[SIG_SZ + 40];
    snprintf(h_key, sizeof(h_key), "KALSHI-ACCESS-KEY: %s",       API_KEY_ID);
    snprintf(h_ts,  sizeof(h_ts),  "KALSHI-ACCESS-TIMESTAMP: %s", ts);
    snprintf(h_sig, sizeof(h_sig), "KALSHI-ACCESS-SIGNATURE: %s", sig);
 
     struct curl_slist *hdrs = NULL;
     hdrs = curl_slist_append(hdrs, h_key);
     hdrs = curl_slist_append(hdrs, h_ts);
     hdrs = curl_slist_append(hdrs, h_sig);
     hdrs = curl_slist_append(hdrs, "Content-Type: application/json");
     hdrs = curl_slist_append(hdrs, "Accept: application/json");
 
     RespBuf *buf = buf_new();
     curl_easy_setopt(curl, CURLOPT_URL,            url);
     curl_easy_setopt(curl, CURLOPT_HTTPHEADER,     hdrs);
     curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION,  write_cb);
     curl_easy_setopt(curl, CURLOPT_WRITEDATA,      buf);
     curl_easy_setopt(curl, CURLOPT_TIMEOUT,        30L);
     curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
 
     if (strcmp(method, "POST") == 0) {
         curl_easy_setopt(curl, CURLOPT_POST,       1L);
         curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body_json ? body_json : "{}");
     } else if (strcmp(method, "DELETE") == 0) {
         curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "DELETE");
     }
 
    CURLcode rc = curl_easy_perform(curl);
    curl_slist_free_all(hdrs);

    /* Reset only the options that vary per-call; keep the connection alive
       so curl can reuse the TCP/TLS connection for the next request. */
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER,    NULL);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS,    NULL);
    curl_easy_setopt(curl, CURLOPT_POST,          0L);
    curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, NULL);

    if (rc != CURLE_OK) {
        fprintf(stderr, "[curl] %s %s → %s\n", method, url, curl_easy_strerror(rc));
        buf_free(buf);
        return NULL;
    }
 
     char *result = strdup(buf->data);
     buf_free(buf);
     return result;
 }
 
 /* ── WebSocket state & callback ──────────────────────────────────────────── */
 
 #define MAX_DELTAS 10       /* max orderbook_delta messages to collect    */
 #define DELTA_TIMEOUT_MS 5000  /* give up waiting for deltas after 5s     */
 
 typedef struct {
     /* auth */
     char  ts[32];
     char  sig[SIG_SZ];
     char  ticker[128];
 
     /* snapshot */
     char *snapshot;
 
    /* ping/pong RTT — 3 pings sent sequentially after snapshot */
#define NUM_PINGS 3
    int       ping_sent;      /* index of next ping to send (0-2) */
    int       pong_count;     /* number of pongs received so far  */
    hr_time_t ping_t0;        /* timestamp of most recent ping    */
    double    ping_rtt_ms[NUM_PINGS]; /* RTT for each ping/pong   */
 
     /* orderbook_delta timing */
     int       delta_count;
     hr_time_t snapshot_t;          /* when snapshot was received            */
     hr_time_t delta_prev;          /* timestamp of last delta (or snapshot) */
     double    snapshot_to_first_ms;/* snapshot receipt → first delta        */
     double    delta_ms[MAX_DELTAS];/* inter-arrival: delta[n-1] → delta[n]  */
 
     /* wall-clock deadline for delta collection (ms since epoch) */
     int64_t   deadline_ms;
 
     /* state machine */
     int       snapshot_done;
     int       done;
 } WsState;
 
 static int ws_cb(struct lws *wsi, enum lws_callback_reasons reason,
                  void *user, void *in, size_t len)
 {
     WsState *st = (WsState *)user;
 
     switch (reason) {
 
     /* Append auth headers to the HTTP Upgrade request */
     case LWS_CALLBACK_CLIENT_APPEND_HANDSHAKE_HEADER: {
         unsigned char **p   = (unsigned char **)in;
         unsigned char  *end = *p + len;
         if (lws_add_http_header_by_name(wsi,
                 (unsigned char *)"KALSHI-ACCESS-KEY:",
                 (unsigned char *)API_KEY_ID, (int)strlen(API_KEY_ID), p, end))
             return -1;
         if (lws_add_http_header_by_name(wsi,
                 (unsigned char *)"KALSHI-ACCESS-TIMESTAMP:",
                 (unsigned char *)st->ts, (int)strlen(st->ts), p, end))
             return -1;
         if (lws_add_http_header_by_name(wsi,
                 (unsigned char *)"KALSHI-ACCESS-SIGNATURE:",
                 (unsigned char *)st->sig, (int)strlen(st->sig), p, end))
             return -1;
         break;
     }
 
     /* Connection established — subscribe to the orderbook channel */
     case LWS_CALLBACK_CLIENT_ESTABLISHED: {
         cJSON *sub  = cJSON_CreateObject();
         cJSON_AddNumberToObject(sub, "id",  1);
         cJSON_AddStringToObject(sub, "cmd", "subscribe");
         cJSON *params   = cJSON_AddObjectToObject(sub, "params");
         cJSON *channels = cJSON_AddArrayToObject(params, "channels");
         cJSON_AddItemToArray(channels, cJSON_CreateString("orderbook_delta"));
         cJSON_AddStringToObject(params, "market_ticker", st->ticker);
 
         char *msg     = cJSON_PrintUnformatted(sub);
         size_t msglen = strlen(msg);
         cJSON_Delete(sub);
 
         unsigned char *wbuf = malloc(LWS_PRE + msglen);
         memcpy(wbuf + LWS_PRE, msg, msglen);
         lws_write(wsi, wbuf + LWS_PRE, msglen, LWS_WRITE_TEXT);
         free(wbuf);
         free(msg);
         break;
     }
 
    /* Writable — send next ping (triggered after snapshot and after each pong) */
    case LWS_CALLBACK_CLIENT_WRITEABLE: {
        if (st->snapshot_done && st->ping_sent < NUM_PINGS) {
            unsigned char ping_buf[LWS_PRE + 4];
            memset(ping_buf, 0, sizeof(ping_buf));
            hr_now(&st->ping_t0);
            lws_write(wsi, ping_buf + LWS_PRE, 0, LWS_WRITE_PING);
            st->ping_sent++;
        }
        break;
    }

    /* Pong received — record RTT and immediately request next ping if needed */
    case LWS_CALLBACK_CLIENT_RECEIVE_PONG: {
        hr_time_t t1;
        hr_now(&t1);
        if (st->pong_count < NUM_PINGS)
            st->ping_rtt_ms[st->pong_count] = hr_ms(&st->ping_t0, &t1);
        st->pong_count++;
        if (st->ping_sent < NUM_PINGS)
            lws_callback_on_writable(wsi);  /* trigger next ping */
        break;
    }
 
     /* Incoming message */
     case LWS_CALLBACK_CLIENT_RECEIVE: {
         cJSON *root = cJSON_ParseWithLength((char *)in, len);
         if (!root) break;
         const char *type =
             cJSON_GetStringValue(cJSON_GetObjectItem(root, "type"));
 
         if (type && strcmp(type, "orderbook_snapshot") == 0) {
             st->snapshot = cJSON_Print(root);
             st->snapshot_done = 1;
             hr_now(&st->snapshot_t);
             st->delta_prev  = st->snapshot_t;
             st->deadline_ms = now_ms() + DELTA_TIMEOUT_MS;
             lws_callback_on_writable(wsi);
 
         } else if (type && strcmp(type, "orderbook_delta") == 0
                    && st->snapshot_done
                    && st->delta_count < MAX_DELTAS) {
             hr_time_t now;
             hr_now(&now);
             if (st->delta_count == 0)
                 st->snapshot_to_first_ms = hr_ms(&st->snapshot_t, &now);
             st->delta_ms[st->delta_count++] = hr_ms(&st->delta_prev, &now);
             st->delta_prev = now;
         }
 
         cJSON_Delete(root);
 
        /* done when: all pongs received + deltas collected (or deadline passed) */
        int pong_ok   = (st->pong_count >= NUM_PINGS);
        int deltas_ok = (st->delta_count >= 3);
        int timed_out = (st->snapshot_done && now_ms() > st->deadline_ms);
        if (st->snapshot_done && pong_ok && (deltas_ok || timed_out))
            st->done = 1;
         break;
     }
 
     case LWS_CALLBACK_CLIENT_CLOSED:
     case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
         st->done = 1;
         break;
 
     default:
         break;
     }
     return 0;
 }
 
 static struct lws_protocols ws_protocols[] = {
     { "kalshi", ws_cb, 0, 256 * 1024, 0, NULL, 0 },
     LWS_PROTOCOL_LIST_TERM
 };
 
 /*
  * Connect, subscribe to orderbook_delta, receive the snapshot, send a ping,
  * collect up to MAX_DELTAS inter-arrival times (or wait DELTA_TIMEOUT_MS),
  * then disconnect.
  */
 static int fetch_orderbook_ws(EVP_PKEY *pkey, const char *ticker, WsState *state)
 {
    memset(state, 0, sizeof(*state));
    for (int i = 0; i < NUM_PINGS; i++) state->ping_rtt_ms[i] = -1.0;
     strncpy(state->ticker, ticker, sizeof(state->ticker));
     state->ticker[sizeof(state->ticker) - 1] = '\0';
     make_auth(pkey, "GET", WS_PATH, state->ts, state->sig);
 
    struct lws_context_creation_info ctx_info = {0};
    ctx_info.port      = CONTEXT_PORT_NO_LISTEN;
    ctx_info.protocols = ws_protocols;
    ctx_info.options   = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT;
    lws_set_log_level(LLL_ERR | LLL_WARN, NULL);
 
     struct lws_context *ctx = lws_create_context(&ctx_info);
     if (!ctx) { fprintf(stderr, "[ws] failed to create context\n"); return -1; }
 
     struct lws_client_connect_info cc = {0};
     cc.context        = ctx;
     cc.address        = WS_HOST;
     cc.port           = WS_PORT;
     cc.path           = WS_PATH;
     cc.host           = WS_HOST;
     cc.origin         = WS_HOST;
     cc.ssl_connection = LCCSCF_USE_SSL;
     cc.protocol       = ws_protocols[0].name;
     cc.userdata       = state;
 
     if (!lws_client_connect_via_info(&cc)) {
         fprintf(stderr, "[ws] connection failed\n");
         lws_context_destroy(ctx);
         return -1;
     }
 
    /* Run until done or hard 15-second wall-clock timeout */
    int64_t deadline = now_ms() + 15000;
    while (!state->done && now_ms() < deadline) {
        lws_service(ctx, 50);
        /* Allow exit once snapshot received even if pong never arrives */
        if (state->snapshot_done && now_ms() > state->deadline_ms)
            break;
    }

    lws_context_destroy(ctx);
    /* Note: lws_context_destroy calls OPENSSL_cleanup() which permanently
     * tears down OpenSSL 3.x. This is intentional — WS is always the last
     * step so no further signing is needed after this point. */
    return state->snapshot ? 0 : -1;
}
 
 /* ── Display helpers ─────────────────────────────────────────────────────── */
 
 static void print_ob_side(cJSON *arr, const char *label)
 {
     printf("  %s bids (price¢ × qty):\n", label);
     if (!cJSON_IsArray(arr)) { printf("    (empty)\n"); return; }
     int n = cJSON_GetArraySize(arr);
     /* show top 10 (highest prices = last entries, sorted ascending) */
     int start = n > 10 ? n - 10 : 0;
     for (int i = n - 1; i >= start; i--) {
         cJSON *lvl = cJSON_GetArrayItem(arr, i);
         if (!cJSON_IsArray(lvl) || cJSON_GetArraySize(lvl) < 2) continue;
         int price = (int)cJSON_GetArrayItem(lvl, 0)->valuedouble;
         int qty   = (int)cJSON_GetArrayItem(lvl, 1)->valuedouble;
         printf("    %3d¢  ×  %d\n", price, qty);
     }
 }
 
 static void print_orderbook(const char *json)
 {
     cJSON *root = cJSON_Parse(json);
     if (!root) { printf("  (parse error)\n"); return; }
     cJSON *msg  = cJSON_GetObjectItem(root, "msg");
     if (!msg) msg = root;
     print_ob_side(cJSON_GetObjectItem(msg, "yes"), "YES");
     print_ob_side(cJSON_GetObjectItem(msg, "no"),  "NO");
     cJSON_Delete(root);
 }
 
 static void print_order(cJSON *order)
 {
     if (!order) return;
     cJSON *id  = cJSON_GetObjectItem(order, "order_id");
     cJSON *st  = cJSON_GetObjectItem(order, "status");
     cJSON *sd  = cJSON_GetObjectItem(order, "side");
     cJSON *ac  = cJSON_GetObjectItem(order, "action");
     cJSON *yp  = cJSON_GetObjectItem(order, "yes_price");
     cJSON *cnt = cJSON_GetObjectItem(order, "initial_count");
     printf("  Order ID  : %s\n", cJSON_GetStringValue(id)  ?: "?");
     printf("  Status    : %s\n", cJSON_GetStringValue(st)  ?: "?");
     printf("  Side      : %s %s\n",
            cJSON_GetStringValue(sd) ?: "?", cJSON_GetStringValue(ac) ?: "?");
     if (yp)  printf("  Yes price : %d¢\n",  (int)yp->valuedouble);
     if (cnt) printf("  Count     : %d\n",   (int)cnt->valuedouble);
 }
 
 /* ── Main ────────────────────────────────────────────────────────────────── */
 
int main(void)
{
    API_KEY_ID = getenv("KALSHI_API_KEY_ID");
    if (!API_KEY_ID) {
        fprintf(stderr, "Error: KALSHI_API_KEY_ID must be set (source .env first)\n");
        return 1;
    }

    curl_global_init(CURL_GLOBAL_DEFAULT);
    CURL *curl = curl_easy_init();
    if (!curl) { fprintf(stderr, "curl init failed\n"); return 1; }

    /* Tune TCP/HTTP for low latency */
    curl_easy_setopt(curl, CURLOPT_TCP_NODELAY,     1L);  /* disable Nagle */
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE,   1L);  /* keep connection alive */
    curl_easy_setopt(curl, CURLOPT_HTTP_VERSION,    CURL_HTTP_VERSION_2TLS); /* force HTTP/2 */
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION,  1L);
    /* Don't silently reuse connections older than 5s — stale connections from
     * the server side cause a hidden retry that adds full TLS handshake cost. */
    curl_easy_setopt(curl, CURLOPT_MAXAGE_CONN,     5L);

    EVP_PKEY *pkey = NULL;
    const char *key_path = getenv("KALSHI_PRIVATE_KEY_PATH");
    if (key_path) {
        /* Read PEM into g_pem_buf so we can reload from memory after
         * lws_context_destroy wipes out BIO_new_file via OPENSSL_cleanup(). */
        FILE *kf = fopen(key_path, "r");
        if (kf) { fread(g_pem_buf, 1, sizeof(g_pem_buf) - 1, kf); fclose(kf); }
        pkey = load_pkey(g_pem_buf);
    } else {
        PRIVATE_KEY_PEM = getenv("KALSHI_PRIVATE_KEY_PEM");
        if (PRIVATE_KEY_PEM) {
            strncpy(g_pem_buf, PRIVATE_KEY_PEM, sizeof(g_pem_buf) - 1);
            pkey = load_pkey(g_pem_buf);
        }
    }
    if (!pkey) {
        fprintf(stderr, "Error: set KALSHI_PRIVATE_KEY_PATH (path to .pem file) "
                        "or KALSHI_PRIVATE_KEY_PEM in your environment\n");
        return 1;
    }
     if (!pkey) { fprintf(stderr, "private key load failed\n"); return 1; }
 
    hr_time_t t0, t1;

    /* Pre-warm the TCP+TLS connection to Kalshi so timed steps don't pay
     * for connection setup. Without this, the first timed call is 3-5x slower. */
    { char *w = http_req(curl, pkey, "GET", "/markets?limit=1", NULL); free(w); }

    /* ── Step 1: find market ─────────────────────────────────────────────── */
     printf("============================================================\n");
     printf(" STEP 1 — Find NYC high-temperature market (series %s)\n", NYC_SERIES);
     printf("============================================================\n");
 
     char *markets_resp = http_req(curl, pkey, "GET",
         "/markets?series_ticker=" NYC_SERIES "&status=open&limit=10", NULL);
     if (!markets_resp) { fprintf(stderr, "markets fetch failed\n"); return 1; }
 
     cJSON *mroot = cJSON_Parse(markets_resp);
     free(markets_resp);
 
     cJSON *mlist = cJSON_GetObjectItem(mroot, "markets");
     if (!mlist) mlist = cJSON_GetObjectItem(mroot, "data");
     if (!cJSON_IsArray(mlist) || cJSON_GetArraySize(mlist) == 0) {
         fprintf(stderr, "No open markets found in %s series\n", NYC_SERIES);
         cJSON_Delete(mroot);
         return 1;
     }
 
     cJSON      *market = cJSON_GetArrayItem(mlist, 0);
     const char *ticker = cJSON_GetStringValue(cJSON_GetObjectItem(market, "ticker"));
     const char *title  = cJSON_GetStringValue(cJSON_GetObjectItem(market, "title"));
     printf("  Ticker : %s\n", ticker ?: "?");
     printf("  Title  : %s\n", title  ?: "?");
 
     char ticker_buf[128] = {0};
     strncpy(ticker_buf, ticker ?: "", sizeof(ticker_buf) - 1);
     cJSON_Delete(mroot);
 
    /* ── Steps 2+3: place and cancel N times to get a latency distribution ── */
#define N_SAMPLES 10
    double place_ms[N_SAMPLES], cancel_ms[N_SAMPLES];
    int    n_ok = 0;

    printf("\n============================================================\n");
    printf(" STEPS 2+3 — Place + Cancel (%d samples)\n", N_SAMPLES);
    printf("============================================================\n");
    printf("  %-4s  %-10s  %-10s\n", "Run", "Place(ms)", "Cancel(ms)");
    printf("  %-4s  %-10s  %-10s\n", "---", "---------", "----------");

    char last_order_id[128] = {0};

    for (int s = 0; s < N_SAMPLES; s++) {
        char coid[48];
        snprintf(coid, sizeof(coid), "c-%lld-%d", (long long)now_ms(), s);
        cJSON *ob = cJSON_CreateObject();
        cJSON_AddStringToObject(ob, "ticker",          ticker_buf);
        cJSON_AddStringToObject(ob, "side",            "yes");
        cJSON_AddStringToObject(ob, "action",          "buy");
        cJSON_AddNumberToObject(ob, "count",           1);
        cJSON_AddNumberToObject(ob, "yes_price",       1);
        cJSON_AddStringToObject(ob, "time_in_force",   "good_till_canceled");
        cJSON_AddStringToObject(ob, "client_order_id", coid);
        char *order_json = cJSON_PrintUnformatted(ob);
        cJSON_Delete(ob);

        hr_now(&t0);
        char *place_resp = http_req(curl, pkey, "POST", "/portfolio/orders", order_json);
        hr_now(&t1);
        place_ms[n_ok] = hr_ms(&t0, &t1);
        free(order_json);

        if (!place_resp) { fprintf(stderr, "  [error] place failed on sample %d\n", s+1); continue; }
        cJSON *pr    = cJSON_Parse(place_resp); free(place_resp);
        cJSON *order = cJSON_GetObjectItem(pr, "order");
        if (!order) order = pr;
        const char *oid = cJSON_GetStringValue(cJSON_GetObjectItem(order, "order_id"));
        char order_id[128] = {0};
        if (oid) strncpy(order_id, oid, sizeof(order_id) - 1);
        strncpy(last_order_id, order_id, sizeof(last_order_id) - 1);
        cJSON_Delete(pr);
        if (!order_id[0]) { fprintf(stderr, "  [error] no order_id on sample %d\n", s+1); continue; }

        char del_path[256];
        snprintf(del_path, sizeof(del_path), "/portfolio/orders/%s", order_id);
        hr_now(&t0);
        char *cancel_resp = http_req(curl, pkey, "DELETE", del_path, NULL);
        hr_now(&t1);
        cancel_ms[n_ok] = hr_ms(&t0, &t1);

        if (!cancel_resp) { fprintf(stderr, "  [error] cancel failed on sample %d\n", s+1); continue; }
        cJSON *cr = cJSON_Parse(cancel_resp); free(cancel_resp);
        cJSON_Delete(cr);

        printf("  %-4d  %-10.1f  %-10.1f\n", s+1, place_ms[n_ok], cancel_ms[n_ok]);
        n_ok++;
    }

    /* compute statistics */
    double p_sum=0, p_min=place_ms[0],  p_max=place_ms[0];
    double c_sum=0, c_min=cancel_ms[0], c_max=cancel_ms[0];
    for (int i = 0; i < n_ok; i++) {
        p_sum += place_ms[i];  if (place_ms[i]  < p_min) p_min = place_ms[i];  if (place_ms[i]  > p_max) p_max = place_ms[i];
        c_sum += cancel_ms[i]; if (cancel_ms[i] < c_min) c_min = cancel_ms[i]; if (cancel_ms[i] > c_max) c_max = cancel_ms[i];
    }
    printf("  %-4s  %-10s  %-10s\n", "---", "---------", "----------");
    printf("  %-4s  %-10.1f  %-10.1f\n", "avg", p_sum/n_ok, c_sum/n_ok);
    printf("  %-4s  %-10.1f  %-10.1f\n", "min", p_min, c_min);
    printf("  %-4s  %-10.1f  %-10.1f\n", "max", p_max, c_max);

    double t_place  = p_sum / n_ok;
    double t_cancel = c_sum / n_ok;
 
     /* ── Step 4: orderbook via WebSocket (last — lws destroys OpenSSL) ───── */
    printf("\n============================================================\n");
    printf(" STEP 4 — Orderbook via WebSocket for %s\n", ticker_buf);
    printf("============================================================\n");

    WsState ws;
    hr_now(&t0);
    int ws_ok = fetch_orderbook_ws(pkey, ticker_buf, &ws);
    hr_now(&t1);
    double t_orderbook = hr_ms(&t0, &t1);

    if (ws_ok == 0 && ws.snapshot) {
        print_orderbook(ws.snapshot);
        free(ws.snapshot);
    } else {
        printf("  (no snapshot received)\n");
    }
    printf("  Snapshot latency (incl. TLS) : %.1f ms\n", t_orderbook);

    if (ws.pong_count > 0) {
        double sum = 0, mn = ws.ping_rtt_ms[0], mx = ws.ping_rtt_ms[0];
        for (int i = 0; i < ws.pong_count; i++) {
            printf("  Ping/pong RTT [%d]            : %.1f ms  (%.1f ms one-way est.)\n",
                   i + 1, ws.ping_rtt_ms[i], ws.ping_rtt_ms[i] / 2.0);
            sum += ws.ping_rtt_ms[i];
            if (ws.ping_rtt_ms[i] < mn) mn = ws.ping_rtt_ms[i];
            if (ws.ping_rtt_ms[i] > mx) mx = ws.ping_rtt_ms[i];
        }
        if (ws.pong_count > 1)
            printf("  Ping/pong avg/min/max        : %.1f / %.1f / %.1f ms\n",
                   sum / ws.pong_count, mn, mx);
    } else {
        printf("  Ping/pong RTT                : (no pongs received)\n");
    }

    if (ws.delta_count > 0) {
        printf("  Snapshot → first delta       : %.1f ms\n", ws.snapshot_to_first_ms);
        double sum = 0, mn = ws.delta_ms[0], mx = ws.delta_ms[0];
        printf("  Delta inter-arrival times    :\n");
        for (int i = 0; i < ws.delta_count; i++) {
            printf("    delta[%d]  %7.1f ms%s\n", i + 1, ws.delta_ms[i],
                   i == 0 ? "  ← snapshot to first delta" : "");
            sum += ws.delta_ms[i];
            if (ws.delta_ms[i] < mn) mn = ws.delta_ms[i];
            if (ws.delta_ms[i] > mx) mx = ws.delta_ms[i];
        }
        printf("  Summary (%d deltas)  : min=%.1f  avg=%.1f  max=%.1f ms\n",
               ws.delta_count, mn, sum / ws.delta_count, mx);
    } else {
        printf("  orderbook_delta              : (none received in %ds — market is quiet)\n",
               DELTA_TIMEOUT_MS / 1000);
    }

    /* ── Timing summary ──────────────────────────────────────────────────── */
     printf("\n============================================================\n");
     printf(" TIMING SUMMARY\n");
     printf("============================================================\n");
     printf("  Orderbook snapshot (WSS) : %8.1f ms  (incl. TLS handshake)\n", t_orderbook);
    if (ws.pong_count > 0) {
        double sum = 0;
        for (int i = 0; i < ws.pong_count; i++) sum += ws.ping_rtt_ms[i];
        printf("  Per-msg RTT (avg %d pings): %8.1f ms  (%.1f ms one-way)\n",
               ws.pong_count, sum / ws.pong_count, sum / ws.pong_count / 2.0);
    }
     printf("  Place order     : %8.1f ms\n", t_place);
     printf("  Cancel order    : %8.1f ms\n", t_cancel);
     printf("  %-42s\n", "------------------------------------------");
     printf("  Total (snapshot + place + cancel): %8.1f ms\n",
            t_orderbook + t_place + t_cancel);
     printf("\nDone. %d/%d place+cancel pairs completed successfully.\n", n_ok, N_SAMPLES);
 
     EVP_PKEY_free(pkey);
     curl_easy_cleanup(curl);
     curl_global_cleanup();
     return 0;
 }
 