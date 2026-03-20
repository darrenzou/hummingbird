/*
 * arb_poly.c
 *
 * Polymarket process for the cross-exchange arbitrage strategy.
 *
 * Flow:
 *   1. Connect WebSocket, wait for initial book snapshot.
 *   2. Send poly_book (bid, ask, volumes) to Kalshi process.
 *   3. Wait for kalshi_signal (arb_ok + kalshi prices).
 *      - If no arb: abort.
 *   4. Pre-sign 20 GTC limit orders (10 bid, 10 ask) using EIP-712.
 *   5. Send poly_signing_done with book volumes.
 *   6. Loop: service WS + poll pipe for KALSHI_FILL / POLY_REDO_SIGNING / ABORT.
 *      - On POLY_REDO_SIGNING: re-sign with updated prices, repeat step 5.
 *      - On KALSHI_FILL: compute how many pre-signed orders to place.
 *      - On ABORT or price threshold (>95% / <5%): cancel all, exit.
 *
 * Crypto dependencies (inlined from poly_test.c):
 *   Keccak-256, ABI encoding, secp256k1 ECDSA, EIP-712 struct hashing,
 *   HMAC-SHA256 L2 authentication.
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_poly.h"
#include "arb_ipc.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <math.h>
#include <signal.h>

/* ── Clean-shutdown signal handling ──────────────────────────────────────── */
static volatile sig_atomic_t g_poly_quit = 0;
static void poly_quit_handler(int sig) { (void)sig; g_poly_quit = 1; }
#include <time.h>
#include <sys/select.h>

#include <curl/curl.h>
#include <libwebsockets.h>
#include <cjson/cJSON.h>

#include <openssl/evp.h>
#include <openssl/hmac.h>
#include <openssl/ec.h>
#include <openssl/ecdsa.h>
#include <openssl/obj_mac.h>
#include <openssl/bn.h>
#include <openssl/bio.h>
#include <openssl/buffer.h>

/* ── Polymarket endpoints ─────────────────────────────────────────────────── */

#define CLOB_BASE        "https://clob.polymarket.com"
#define WS_HOST          "ws-subscriptions-clob.polymarket.com"
#define WS_PATH          "/ws/market"
#define WS_PORT          443

/* CTF Exchange contracts on Polygon */
#define CTF_EXCHANGE     "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
#define CTF_EXCHANGE_NEG "0xC5d563A36AE78145C45a50134d48A1215220f80a"
#define CHAIN_ID         137u
#define SIG_TYPE         1u          /* POLY_PROXY */
#define FEE_RATE_BPS     "0"
#define SIDE_BUY         0

/* Abort threshold: cancel and exit if YES price is outside this band */
#define ABORT_HIGH       0.95
#define ABORT_LOW        0.05

/* Binary pre-signed order pool.
 * Slot i covers exactly 2^i contracts.  n_bits slots represent any fill
 * up to (2^n_bits - 1) using at most n_bits orders — one per set bit. */
#define MAX_BITS        20           /* supports volumes up to 2^20 ≈ 1 M */
#define MAX_ORDER_BODY  2048
#define MAX_PLACED_IDS  1024         /* running cancel buffer */

/* Orderbook level tracking (enough for abort monitoring) */
#define MAX_OB_LEVELS    64

/* Ping interval for WS keep-alive (ms) */
#define PING_INTERVAL_MS 25000

/* ── Credentials (set by poly_run before WS connect) ─────────────────────── */

static const char *g_poly_address;
static const char *g_poly_api_key;
static const char *g_poly_secret;
static const char *g_poly_pass;
static const char *g_eth_priv_key;

/* ── Compact Keccak-256 ───────────────────────────────────────────────────── */

#define KECCAK_RATE 136

static const uint64_t krc[24] = {
    0x0000000000000001ULL,0x0000000000008082ULL,0x800000000000808aULL,
    0x8000000080008000ULL,0x000000000000808bULL,0x0000000080000001ULL,
    0x8000000080008081ULL,0x8000000000008009ULL,0x000000000000008aULL,
    0x0000000000000088ULL,0x0000000080008009ULL,0x000000008000000aULL,
    0x000000008000808bULL,0x800000000000008bULL,0x8000000000008089ULL,
    0x8000000000008003ULL,0x8000000000008002ULL,0x8000000000000080ULL,
    0x000000000000800aULL,0x800000008000000aULL,0x8000000080008081ULL,
    0x8000000000008080ULL,0x0000000080000001ULL,0x8000000080008008ULL
};
static const int kro[24] = {1,62,28,27,36,44,6,55,20,3,10,43,25,39,41,45,15,21,8,18,2,61,56,14};
static const int kpi[24] = {10,7,11,17,18,3,5,16,8,21,24,4,15,23,19,13,12,2,20,14,22,9,6,1};

#define ROT64(x,n) (((x)<<(n))|((x)>>(64-(n))))

static void keccakf(uint64_t s[25]) {
    uint64_t t, bc[5];
    for (int r = 0; r < 24; r++) {
        for (int i = 0; i < 5; i++)
            bc[i] = s[i] ^ s[i+5] ^ s[i+10] ^ s[i+15] ^ s[i+20];
        for (int i = 0; i < 5; i++) {
            t = bc[(i+4)%5] ^ ROT64(bc[(i+1)%5], 1);
            for (int j = 0; j < 25; j += 5) s[j+i] ^= t;
        }
        t = s[1];
        for (int i = 0; i < 24; i++) {
            int j = kpi[i]; bc[0] = s[j]; s[j] = ROT64(t, kro[i]); t = bc[0];
        }
        for (int j = 0; j < 25; j += 5) {
            uint64_t tmp[5];
            for (int i = 0; i < 5; i++) tmp[i] = s[j+i];
            for (int i = 0; i < 5; i++) s[j+i] ^= (~tmp[(i+1)%5]) & tmp[(i+2)%5];
        }
        s[0] ^= krc[r];
    }
}

static void keccak256(const uint8_t *in, size_t len, uint8_t out[32]) {
    uint64_t st[25] = {0};
    uint8_t *b = (uint8_t *)st;
    for (; len >= KECCAK_RATE; len -= KECCAK_RATE, in += KECCAK_RATE) {
        for (int i = 0; i < KECCAK_RATE; i++) b[i] ^= in[i];
        keccakf(st);
    }
    uint8_t pad[KECCAK_RATE] = {0};
    memcpy(pad, in, len);
    pad[len]            = 0x01;
    pad[KECCAK_RATE-1] |= 0x80;
    for (int i = 0; i < KECCAK_RATE; i++) b[i] ^= pad[i];
    keccakf(st);
    memcpy(out, st, 32);
}

/* ── Base64 helpers ───────────────────────────────────────────────────────── */

static char *b64_encode(const uint8_t *data, size_t len) {
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

static size_t b64_decode(const char *in, uint8_t *out, size_t max) {
    BIO *b64 = BIO_new(BIO_f_base64());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    BIO *mem = BIO_new_mem_buf(in, -1);
    BIO_push(b64, mem);
    int n = BIO_read(b64, out, (int)max);
    BIO_free_all(b64);
    return (n > 0) ? (size_t)n : 0;
}

static void bytes_to_hex(const uint8_t *src, size_t len, char *dst) {
    for (size_t i = 0; i < len; i++)
        snprintf(dst + i*2, 3, "%02x", src[i]);
}

/* ── ABI encoding helpers ─────────────────────────────────────────────────── */

static void abi_dec(uint8_t buf[32], const char *dec) {
    BIGNUM *bn = BN_new(); BN_dec2bn(&bn, dec);
    memset(buf, 0, 32); BN_bn2binpad(bn, buf, 32); BN_free(bn);
}
static void abi_u64(uint8_t buf[32], uint64_t v) {
    memset(buf, 0, 32);
    for (int i = 7; i >= 0; i--) { buf[24+i] = (uint8_t)(v & 0xFF); v >>= 8; }
}
static void abi_addr(uint8_t buf[32], const char *hex) {
    BIGNUM *bn = BN_new();
    const char *p = (strncmp(hex, "0x", 2) == 0) ? hex+2 : hex;
    BN_hex2bn(&bn, p); memset(buf, 0, 32); BN_bn2binpad(bn, buf, 32); BN_free(bn);
}
static void abi_str(uint8_t buf[32], const char *str) {
    keccak256((const uint8_t *)str, strlen(str), buf);
}

/* ── secp256k1 ECDSA with recovery bit ───────────────────────────────────── */

static int eth_sign(const uint8_t digest[32], const char *priv_hex,
                    uint8_t r_out[32], uint8_t s_out[32], uint8_t *v_out)
{
    EC_KEY *key = EC_KEY_new_by_curve_name(NID_secp256k1);
    BIGNUM *priv = BN_new();
    const char *p = (strncmp(priv_hex, "0x", 2) == 0) ? priv_hex+2 : priv_hex;
    BN_hex2bn(&priv, p);
    EC_KEY_set_private_key(key, priv);

    const EC_GROUP *grp = EC_KEY_get0_group(key);
    EC_POINT *pub = EC_POINT_new(grp);
    EC_POINT_mul(grp, pub, priv, NULL, NULL, NULL);
    EC_KEY_set_public_key(key, pub);

    ECDSA_SIG *sig = ECDSA_do_sign(digest, 32, key);
    if (!sig) { EC_KEY_free(key); BN_free(priv); EC_POINT_free(pub); return 0; }

    const BIGNUM *r, *s;
    ECDSA_SIG_get0(sig, &r, &s);
    BN_bn2binpad(r, r_out, 32);
    BN_bn2binpad(s, s_out, 32);

    BN_CTX *ctx = BN_CTX_new();
    BIGNUM *order = BN_new();
    EC_GROUP_get_order(grp, order, ctx);

    *v_out = 27;
    for (int recid = 0; recid <= 1; recid++) {
        BIGNUM *rx = BN_dup(r);
        EC_POINT *R = EC_POINT_new(grp);
        if (EC_POINT_set_compressed_coordinates(grp, R, rx, recid & 1, ctx)) {
            BIGNUM *h    = BN_bin2bn(digest, 32, NULL);
            BIGNUM *rinv = BN_new(); BN_mod_inverse(rinv, r, order, ctx);
            BIGNUM *u1   = BN_new(); BN_zero(u1);
            BIGNUM *tmp  = BN_new(); BN_mod_mul(tmp, h, rinv, order, ctx);
            BN_mod_sub(u1, u1, tmp, order, ctx);
            BIGNUM *u2   = BN_new(); BN_mod_mul(u2, s, rinv, order, ctx);
            EC_POINT *Q  = EC_POINT_new(grp);
            EC_POINT_mul(grp, Q, u1, R, u2, ctx);
            if (EC_POINT_cmp(grp, Q, pub, ctx) == 0) {
                *v_out = (uint8_t)(27 + recid);
                EC_POINT_free(Q); BN_free(h); BN_free(rinv);
                BN_free(u1); BN_free(tmp); BN_free(u2);
                EC_POINT_free(R); BN_free(rx);
                break;
            }
            EC_POINT_free(Q); BN_free(h); BN_free(rinv);
            BN_free(u1); BN_free(tmp); BN_free(u2);
        }
        EC_POINT_free(R); BN_free(rx);
    }

    BN_free(order); BN_CTX_free(ctx);
    ECDSA_SIG_free(sig);
    EC_KEY_free(key); BN_free(priv); EC_POINT_free(pub);
    return 1;
}

/* ── EIP-712 Order signing ────────────────────────────────────────────────── */

/*
 * Sign a Polymarket limit order using EIP-712.
 *
 * token_id      – YES-token uint256 as decimal string
 * maker         – 0x proxy wallet address
 * salt          – unique decimal nonce string (prevents replay)
 * maker_amount  – USDC collateral in 1e-6 units (decimal string)
 * taker_amount  – outcome tokens in 1e-6 units (decimal string)
 * side          – 0 = BUY
 * neg_risk      – 1 = use negRisk CTF exchange address
 * sig_hex       – output: "0x" + 64r + 64s + 02v (135 bytes including NUL)
 */
static void sign_order(const char *token_id, const char *maker,
                       const char *salt, const char *maker_amount,
                       const char *taker_amount, int side, int neg_risk,
                       char sig_hex[135])
{
    static const char domain_type_str[] =
        "EIP712Domain(string name,string version,uint256 chainId,"
        "address verifyingContract)";
    static const char order_type_str[] =
        "Order(uint256 salt,address maker,address signer,address taker,"
        "uint256 tokenId,uint256 makerAmount,uint256 takerAmount,"
        "uint256 expiration,uint256 nonce,uint256 feeRateBps,"
        "uint8 side,uint8 signatureType)";

    uint8_t domain_type_hash[32], order_type_hash[32];
    keccak256((const uint8_t *)domain_type_str, strlen(domain_type_str), domain_type_hash);
    keccak256((const uint8_t *)order_type_str,  strlen(order_type_str),  order_type_hash);

    /* Domain separator */
    const char *exchange = neg_risk ? CTF_EXCHANGE_NEG : CTF_EXCHANGE;
    uint8_t domain_enc[5*32];
    uint8_t *dp = domain_enc;
    memcpy(dp, domain_type_hash, 32); dp += 32;
    abi_str(dp, "CTF Exchange");        dp += 32;
    abi_str(dp, "1");                   dp += 32;
    abi_u64(dp, CHAIN_ID);              dp += 32;
    abi_addr(dp, exchange);             dp += 32;

    uint8_t domain_sep[32];
    keccak256(domain_enc, sizeof(domain_enc), domain_sep);

    /* Struct hash */
    uint8_t struct_enc[13*32];
    uint8_t *sp = struct_enc;
    memcpy(sp, order_type_hash, 32);                                    sp += 32;
    abi_dec(sp, salt);                                                  sp += 32;
    abi_addr(sp, maker);                                                sp += 32;
    abi_addr(sp, maker);                                                sp += 32; /* signer = maker */
    abi_addr(sp, "0x0000000000000000000000000000000000000000");         sp += 32;
    abi_dec(sp, token_id);                                              sp += 32;
    abi_dec(sp, maker_amount);                                          sp += 32;
    abi_dec(sp, taker_amount);                                          sp += 32;
    abi_u64(sp, 0);                                                     sp += 32; /* expiration */
    abi_u64(sp, 0);                                                     sp += 32; /* nonce      */
    abi_dec(sp, FEE_RATE_BPS);                                          sp += 32;
    abi_u64(sp, (uint64_t)side);                                        sp += 32;
    abi_u64(sp, SIG_TYPE);                                              sp += 32;

    uint8_t struct_hash[32];
    keccak256(struct_enc, sizeof(struct_enc), struct_hash);

    /* Final EIP-191 / EIP-712 digest */
    uint8_t pre[66];
    pre[0] = 0x19; pre[1] = 0x01;
    memcpy(pre + 2,  domain_sep,  32);
    memcpy(pre + 34, struct_hash, 32);

    uint8_t digest[32];
    keccak256(pre, 66, digest);

    uint8_t r[32], s[32], v;
    if (!eth_sign(digest, g_eth_priv_key, r, s, &v)) {
        fprintf(stderr, "[poly] eth_sign failed\n");
        strcpy(sig_hex, "0x");
        return;
    }

    sig_hex[0] = '0'; sig_hex[1] = 'x';
    bytes_to_hex(r, 32, sig_hex + 2);
    bytes_to_hex(s, 32, sig_hex + 66);
    snprintf(sig_hex + 130, 5, "%02x", v);
}

/* ── HMAC-SHA256 L2 authentication ───────────────────────────────────────── */

static char *hmac_l2_sig(const char *secret_b64, const char *ts,
                         const char *method, const char *path, const char *body)
{
    uint8_t key[128];
    size_t  klen = b64_decode(secret_b64, key, sizeof(key));

    char msg[8192];
    snprintf(msg, sizeof(msg), "%s%s%s%s", ts, method, path, body ? body : "");

    uint8_t     digest[32];
    unsigned int dlen = 32;
    HMAC(EVP_sha256(), key, (int)klen,
         (const uint8_t *)msg, strlen(msg), digest, &dlen);
    return b64_encode(digest, dlen);
}

static struct curl_slist *l2_headers(const char *method, const char *path,
                                      const char *body)
{
    char ts[32];
    snprintf(ts, sizeof(ts), "%lld", (long long)(now_ms() / 1000));

    char *sig = hmac_l2_sig(g_poly_secret, ts, method, path, body);

    char hdr[640];
    struct curl_slist *sl = NULL;

    snprintf(hdr, sizeof(hdr), "POLY_ADDRESS: %s",    g_poly_address); sl = curl_slist_append(sl, hdr);
    snprintf(hdr, sizeof(hdr), "POLY_SIGNATURE: %s",  sig);            sl = curl_slist_append(sl, hdr);
    snprintf(hdr, sizeof(hdr), "POLY_TIMESTAMP: %s",  ts);             sl = curl_slist_append(sl, hdr);
    snprintf(hdr, sizeof(hdr), "POLY_API_KEY: %s",    g_poly_api_key); sl = curl_slist_append(sl, hdr);
    snprintf(hdr, sizeof(hdr), "POLY_PASSPHRASE: %s", g_poly_pass);    sl = curl_slist_append(sl, hdr);
    free(sig);
    sl = curl_slist_append(sl, "Content-Type: application/json");
    return sl;
}

/* ── HTTP helper ──────────────────────────────────────────────────────────── */

typedef struct { char *buf; size_t len; } RespBuf;

static size_t write_cb(void *data, size_t sz, size_t nmemb, void *userp) {
    RespBuf *r = (RespBuf *)userp;
    size_t total = sz * nmemb;
    r->buf = realloc(r->buf, r->len + total + 1);
    memcpy(r->buf + r->len, data, total);
    r->len += total;
    r->buf[r->len] = '\0';
    return total;
}

/* Returns heap-allocated response string; caller frees.  NULL on error. */
static char *http_req(CURL *curl, const char *method, const char *url,
                      const char *body, struct curl_slist *hdrs)
{
    RespBuf resp = {NULL, 0};
    curl_easy_setopt(curl, CURLOPT_URL,           url);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION,  write_cb);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA,      &resp);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER,     hdrs);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);

    if (strcmp(method, "POST") == 0) {
        curl_easy_setopt(curl, CURLOPT_POST, 1L);
        curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body ? body : "");
    } else if (strcmp(method, "DELETE") == 0) {
        curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "DELETE");
        curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body ? body : "");
    } else {
        curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
    }

    curl_easy_perform(curl);
    curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, NULL);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS,    NULL);
    curl_easy_setopt(curl, CURLOPT_POST,          0L);
    return resp.buf ? resp.buf : strdup("");
}

/* ── Orderbook level tracking for abort monitoring ───────────────────────── */

typedef struct { float price; float size; } ObLevel;

typedef struct {
    ObLevel bids[MAX_OB_LEVELS]; int n_bids;  /* sorted desc by price */
    ObLevel asks[MAX_OB_LEVELS]; int n_asks;  /* sorted asc  by price */
} Orderbook;

static void ob_update(Orderbook *ob, float price, int is_bid, float size)
{
    ObLevel *arr = is_bid ? ob->bids : ob->asks;
    int     *cnt = is_bid ? &ob->n_bids : &ob->n_asks;

    /* find existing level */
    for (int i = 0; i < *cnt; i++) {
        if (fabsf(arr[i].price - price) < 0.0001f) {
            if (size <= 0.0f) {
                /* remove */
                memmove(&arr[i], &arr[i+1], (size_t)(*cnt - i - 1) * sizeof(ObLevel));
                (*cnt)--;
            } else {
                arr[i].size = size;
            }
            return;
        }
    }
    /* insert new level (if room and size > 0) */
    if (size <= 0.0f || *cnt >= MAX_OB_LEVELS) return;
    arr[*cnt].price = price;
    arr[*cnt].size  = size;
    (*cnt)++;
    /* keep sorted */
    if (is_bid) {
        for (int i = *cnt - 1; i > 0 && arr[i].price > arr[i-1].price; i--) {
            ObLevel tmp = arr[i]; arr[i] = arr[i-1]; arr[i-1] = tmp;
        }
    } else {
        for (int i = *cnt - 1; i > 0 && arr[i].price < arr[i-1].price; i--) {
            ObLevel tmp = arr[i]; arr[i] = arr[i-1]; arr[i-1] = tmp;
        }
    }
}

/* ── WebSocket state ──────────────────────────────────────────────────────── */

typedef struct {
    int       got_snapshot;
    int       done;
    int       abort_triggered;

    float     best_bid;
    float     best_ask;
    float     best_bid_vol;
    float     best_ask_vol;

    Orderbook ob;

    /* keep-alive */
    int64_t   last_ping_ms;
    int       send_ping;

    /* token subscribed (for mismatch check) */
    char      token_id[MAX_TOKEN_ID_LEN];

    struct lws *wsi;
} PolyWsCtx;

static PolyWsCtx g_ws;

/* ── WebSocket callback ───────────────────────────────────────────────────── */

static int poly_ws_cb(struct lws *wsi, enum lws_callback_reasons reason,
                      void *user, void *in, size_t len)
{
    (void)user;

    switch (reason) {

    case LWS_CALLBACK_CLIENT_ESTABLISHED: {
        g_ws.wsi = wsi;
        /* Subscribe to the market channel for our token */
        char sub[512];
        snprintf(sub, sizeof(sub),
                 "{\"assets_ids\":[\"%s\"],\"type\":\"market\","
                 "\"custom_feature_enabled\":true}",
                 g_ws.token_id);
        size_t slen = strlen(sub);
        uint8_t *buf = malloc(LWS_PRE + slen);
        memcpy(buf + LWS_PRE, sub, slen);
        lws_write(wsi, buf + LWS_PRE, slen, LWS_WRITE_TEXT);
        free(buf);
        g_ws.last_ping_ms = now_ms();
        break;
    }

    case LWS_CALLBACK_CLIENT_WRITEABLE: {
        if (g_ws.send_ping) {
            g_ws.send_ping = 0;
            const char *ping = "PING";
            uint8_t buf[LWS_PRE + 4];
            memcpy(buf + LWS_PRE, ping, 4);
            lws_write(wsi, buf + LWS_PRE, 4, LWS_WRITE_TEXT);
            g_ws.last_ping_ms = now_ms();
        }
        break;
    }

    case LWS_CALLBACK_CLIENT_RECEIVE: {
        const char *data = (const char *)in;

        /* Polymarket text heartbeat */
        if (len == 4 && strncmp(data, "PONG", 4) == 0)
            break;

        cJSON *msg = cJSON_ParseWithLength(data, len);
        if (!msg) break;

        /* Messages can be an array of objects or a single object */
        cJSON *arr  = cJSON_IsArray(msg) ? msg : NULL;
        int    alen = arr ? cJSON_GetArraySize(arr) : 1;

        for (int mi = 0; mi < alen; mi++) {
            cJSON *m = arr ? cJSON_GetArrayItem(arr, mi) : msg;
            if (!m) continue;

            const char *mtype = "";
            cJSON *tj = cJSON_GetObjectItem(m, "type");
            if (tj && cJSON_IsString(tj)) mtype = tj->valuestring;

            /* Initial book snapshot */
            int is_book = (!g_ws.got_snapshot &&
                           (cJSON_GetObjectItem(m, "bids") ||
                            cJSON_GetObjectItem(m, "asks")) &&
                           (strcmp(mtype, "") == 0 || strcmp(mtype, "book") == 0));

            if (is_book) {
                g_ws.got_snapshot = 1;

                cJSON *bids = cJSON_GetObjectItem(m, "bids");
                cJSON *asks = cJSON_GetObjectItem(m, "asks");

                /* Populate orderbook and extract best levels */
                if (bids && cJSON_IsArray(bids)) {
                    int nb = cJSON_GetArraySize(bids);
                    /* Bids are highest-first */
                    for (int i = 0; i < nb && i < MAX_OB_LEVELS; i++) {
                        cJSON *b = cJSON_GetArrayItem(bids, i);
                        float pr = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(b, "price")) ?: "0");
                        float sz = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(b, "size"))  ?: "0");
                        ob_update(&g_ws.ob, pr, 1, sz);
                    }
                    cJSON *b0 = cJSON_GetArrayItem(bids, 0);
                    if (b0) {
                        g_ws.best_bid     = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(b0, "price")) ?: "0");
                        g_ws.best_bid_vol = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(b0, "size"))  ?: "0");
                    }
                }
                if (asks && cJSON_IsArray(asks)) {
                    int na = cJSON_GetArraySize(asks);
                    /* Asks are lowest-first */
                    for (int i = 0; i < na && i < MAX_OB_LEVELS; i++) {
                        cJSON *a = cJSON_GetArrayItem(asks, i);
                        float pr = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(a, "price")) ?: "0");
                        float sz = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(a, "size"))  ?: "0");
                        ob_update(&g_ws.ob, pr, 0, sz);
                    }
                    cJSON *a0 = cJSON_GetArrayItem(asks, 0);
                    if (a0) {
                        g_ws.best_ask     = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(a0, "price")) ?: "0");
                        g_ws.best_ask_vol = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(a0, "size"))  ?: "0");
                    }
                }

            } else if (strcmp(mtype, "price_change") == 0 && g_ws.got_snapshot) {
                /* Apply delta updates to local orderbook */
                cJSON *changes = cJSON_GetObjectItem(m, "changes");
                if (!cJSON_IsArray(changes)) break;
                int nc = cJSON_GetArraySize(changes);
                for (int i = 0; i < nc; i++) {
                    cJSON *c = cJSON_GetArrayItem(changes, i);
                    const char *side_str = cJSON_GetStringValue(cJSON_GetObjectItem(c, "side")) ?: "";
                    float pr   = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(c, "price")) ?: "0");
                    float sz   = (float)atof(cJSON_GetStringValue(cJSON_GetObjectItem(c, "size"))  ?: "0");
                    int is_bid = (strcmp(side_str, "BUY") == 0);
                    ob_update(&g_ws.ob, pr, is_bid, sz);
                }
                /* Refresh best bid/ask */
                if (g_ws.ob.n_bids > 0) {
                    g_ws.best_bid     = g_ws.ob.bids[0].price;
                    g_ws.best_bid_vol = g_ws.ob.bids[0].size;
                }
                if (g_ws.ob.n_asks > 0) {
                    g_ws.best_ask     = g_ws.ob.asks[0].price;
                    g_ws.best_ask_vol = g_ws.ob.asks[0].size;
                }
                /* Check abort threshold */
                if (g_ws.best_bid > ABORT_HIGH || g_ws.best_ask < ABORT_LOW)
                    g_ws.abort_triggered = 1;
            }
        }

        cJSON_Delete(msg);
        break;
    }

    case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
        fprintf(stderr, "[poly-ws] connection error: %s\n",
                in ? (char *)in : "(unknown)");
        g_ws.done = 1;
        break;

    case LWS_CALLBACK_CLIENT_CLOSED:
        fprintf(stderr, "[poly-ws] connection closed\n");
        g_ws.done = 1;
        break;

    default:
        break;
    }
    return 0;
}

static struct lws_protocols poly_protocols[] = {
    {"polymarket-market", poly_ws_cb, 0, 256*1024, 0, NULL, 0},
    LWS_PROTOCOL_LIST_TERM
};

/* ── Connect and subscribe ────────────────────────────────────────────────── */

static struct lws_context *poly_ws_connect(const char *token_id)
{
    memset(&g_ws, 0, sizeof(g_ws));
    snprintf(g_ws.token_id, sizeof(g_ws.token_id), "%s", token_id);

    struct lws_context_creation_info ci = {0};
    ci.port      = CONTEXT_PORT_NO_LISTEN;
    ci.protocols = poly_protocols;
    ci.options   = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT
                 | LWS_SERVER_OPTION_DISABLE_IPV6;
    ci.ssl_ca_filepath = "/etc/pki/tls/certs/ca-bundle.crt";
    lws_set_log_level(LLL_ERR | LLL_WARN, NULL);

    struct lws_context *ctx = lws_create_context(&ci);
    if (!ctx) { fprintf(stderr, "[poly-ws] lws_create_context failed\n"); return NULL; }

    struct lws_client_connect_info cc = {0};
    cc.context        = ctx;
    cc.address        = WS_HOST;
    cc.port           = WS_PORT;
    cc.path           = WS_PATH;
    cc.host           = WS_HOST;
    cc.origin         = WS_HOST;
    cc.protocol       = NULL;
    cc.ssl_connection = LCCSCF_USE_SSL;
    cc.userdata       = NULL;

    if (!lws_client_connect_via_info(&cc)) {
        fprintf(stderr, "[poly-ws] connect failed\n");
        lws_context_destroy(ctx);
        return NULL;
    }
    return ctx;
}

/* ── Pre-signed order storage ─────────────────────────────────────────────── */

typedef struct { char body[MAX_ORDER_BODY]; } SignedOrder;

/* Slot i is for 2^i contracts.  Re-signed in-place after each use. */
static SignedOrder g_bid_orders[MAX_BITS];
static SignedOrder g_ask_orders[MAX_BITS];
static int         g_bid_n_bits = 0;
static int         g_ask_n_bits = 0;

/* Stored when presign_orders runs; reused to re-sign slots after fills. */
static const char *g_resign_token_id = NULL;
static double      g_resign_bid_price = 0.0;
static double      g_resign_ask_price = 0.0;
static int         g_resign_neg_risk  = 0;

/* IDs of placed Poly orders so we can cancel them on abort */
static char g_placed_ids[MAX_PLACED_IDS][64];
static int  g_n_placed_ids = 0;

/*
 * Polymarket portfolio counters (contract units, integer approximation).
 *
 * Bid-pool orders (placed when Kalshi's BID fills, i.e. Kalshi BOUGHT YES)
 * hedge by obtaining NO exposure on Polymarket -> tracked in g_no_held.
 *
 * Ask-pool orders (placed when Kalshi's ASK fills, i.e. Kalshi SOLD YES)
 * hedge by buying YES on Polymarket -> tracked in g_yes_held.
 *
 * When both counters are positive, YES+NO pairs can be merged/redeemed
 * for $1 each (they offset each other).  We reduce both by min(yes,no).
 */
static int g_yes_held = 0;
static int g_no_held  = 0;

/* ── Generate a random decimal salt ──────────────────────────────────────── */

static void gen_salt(char *buf, size_t sz)
{
    uint64_t a = (uint64_t)rand() << 32 | (uint64_t)rand();
    snprintf(buf, sz, "%llu", (unsigned long long)a);
}

/* ── Build a POST /order JSON body from a signed order ───────────────────── */

static void build_order_body(char *out, size_t outsz,
                              const char *salt, const char *maker,
                              const char *token_id,
                              const char *maker_amount, const char *taker_amount,
                              int side, const char *sig)
{
    cJSON *order = cJSON_CreateObject();
    cJSON_AddStringToObject(order, "salt",          salt);
    cJSON_AddStringToObject(order, "maker",         maker);
    cJSON_AddStringToObject(order, "signer",        maker);
    cJSON_AddStringToObject(order, "taker",
                            "0x0000000000000000000000000000000000000000");
    cJSON_AddStringToObject(order, "tokenId",       token_id);
    cJSON_AddStringToObject(order, "makerAmount",   maker_amount);
    cJSON_AddStringToObject(order, "takerAmount",   taker_amount);
    cJSON_AddStringToObject(order, "expiration",    "0");
    cJSON_AddStringToObject(order, "nonce",         "0");
    cJSON_AddStringToObject(order, "feeRateBps",    FEE_RATE_BPS);
    cJSON_AddNumberToObject(order, "side",          side);
    cJSON_AddNumberToObject(order, "signatureType", (int)SIG_TYPE);
    cJSON_AddStringToObject(order, "signature",     sig);

    cJSON *body = cJSON_CreateObject();
    cJSON_AddItemToObject(body, "order",     order);
    cJSON_AddStringToObject(body, "owner",     maker);
    cJSON_AddStringToObject(body, "orderType", "GTC");

    char *s = cJSON_PrintUnformatted(body);
    strncpy(out, s, outsz - 1);
    free(s);
    cJSON_Delete(body);
}

/* ── Sign a single binary slot ────────────────────────────────────────────── */

/*
 * Sign and store one order for slot_idx, which covers 2^slot_idx contracts.
 * Called during initial presign and after each fill to re-arm that slot.
 */
static void sign_binary_slot(int slot_idx, double price, int side, int neg_risk,
                              const char *token_id, SignedOrder *out)
{
    uint64_t vol_units = (uint64_t)1 << slot_idx;
    uint64_t taker_amt = vol_units * 1000000ULL;
    uint64_t maker_amt = (uint64_t)round(price * (double)vol_units * 1e6);

    char taker_s[32], maker_s[32], salt[32], sig[135];
    snprintf(taker_s, sizeof(taker_s), "%llu", (unsigned long long)taker_amt);
    snprintf(maker_s, sizeof(maker_s), "%llu", (unsigned long long)maker_amt);

    gen_salt(salt, sizeof(salt));
    sign_order(token_id, g_poly_address, salt, maker_s, taker_s,
               side, neg_risk, sig);
    build_order_body(out->body, MAX_ORDER_BODY,
                     salt, g_poly_address, token_id,
                     maker_s, taker_s, side, sig);
}

/* ── Pre-sign binary slot orders ─────────────────────────────────────────── */

/*
 * Binary slot signing.
 *
 * Converts bid_vol and ask_vol to binary and signs one order per bit,
 * where slot i covers exactly 2^i contracts.  Any fill F can be hedged
 * by submitting the slots whose indices are set in the binary form of F,
 * using at most n_bits orders.  Submitted slots are immediately re-signed
 * for the next fill.
 */
static void presign_orders(const char *token_id, int kalshi_bid, int kalshi_ask,
                           double bid_vol, double ask_vol, int neg_risk)
{
    double bid_price = 1.0 - kalshi_bid / 100.0;
    double ask_price = 1.0 - kalshi_ask / 100.0;

    /* Store signing context for re-signing after fills */
    g_resign_token_id  = token_id;
    g_resign_bid_price = bid_price;
    g_resign_ask_price = ask_price;
    g_resign_neg_risk  = neg_risk;

    /* n_bits = floor(log2(vol)) + 1  (0 if vol < 1) */
    g_bid_n_bits = (bid_vol >= 1.0) ? (int)floor(log2(bid_vol)) + 1 : 0;
    g_ask_n_bits = (ask_vol >= 1.0) ? (int)floor(log2(ask_vol)) + 1 : 0;
    if (g_bid_n_bits > MAX_BITS) g_bid_n_bits = MAX_BITS;
    if (g_ask_n_bits > MAX_BITS) g_ask_n_bits = MAX_BITS;

    for (int i = 0; i < g_bid_n_bits; i++)
        sign_binary_slot(i, bid_price, SIDE_BUY, neg_risk, token_id,
                         &g_bid_orders[i]);
    for (int i = 0; i < g_ask_n_bits; i++)
        sign_binary_slot(i, ask_price, SIDE_BUY, neg_risk, token_id,
                         &g_ask_orders[i]);

    printf("[poly] Binary pre-sign: bid=%d slots (1..%llu) ask=%d slots (1..%llu) "
           "@ bid_price=%.4f ask_price=%.4f\n",
           g_bid_n_bits,
           g_bid_n_bits > 0 ? ((uint64_t)1 << (g_bid_n_bits - 1)) : 0ULL,
           g_ask_n_bits,
           g_ask_n_bits > 0 ? ((uint64_t)1 << (g_ask_n_bits - 1)) : 0ULL,
           bid_price, ask_price);
}

/* ── Fetch available Polymarket USDC balance ──────────────────────────────── */

/*
 * GET /accounts with L2 auth.
 * Returns the available USDC balance in whole dollars (0.0 on error).
 * The CLOB API returns balance as a decimal string in USDC units.
 */
static double fetch_poly_balance(CURL *curl)
{
    struct curl_slist *hdrs = l2_headers("GET", "/accounts", "");
    char *resp = http_req(curl, "GET", CLOB_BASE "/accounts", NULL, hdrs);
    curl_slist_free_all(hdrs);
    if (!resp) return 0.0;

    double balance = 0.0;
    cJSON *root = cJSON_Parse(resp);
    free(resp);
    if (!root) return 0.0;

    /* Response may be an array of account objects or a single object */
    cJSON *obj = cJSON_IsArray(root) ? cJSON_GetArrayItem(root, 0) : root;
    if (obj) {
        cJSON *bal = cJSON_GetObjectItem(obj, "balance");
        if (bal) {
            if (cJSON_IsString(bal))
                balance = atof(bal->valuestring);
            else if (cJSON_IsNumber(bal))
                balance = bal->valuedouble;
        }
    }
    cJSON_Delete(root);
    printf("[poly] available USDC balance: $%.2f\n", balance);
    return balance;
}

/* ── Place one pre-signed order via REST ──────────────────────────────────── */

static void place_signed_order(CURL *curl, const char *body)
{
    struct curl_slist *hdrs = l2_headers("POST", "/order", body);
    char *resp = http_req(curl, "POST", CLOB_BASE "/order", body, hdrs);
    curl_slist_free_all(hdrs);

    if (!resp) { fprintf(stderr, "[poly] place order: no response\n"); return; }

    cJSON *pr = cJSON_Parse(resp);
    free(resp);
    if (!pr) return;

    const char *oid = NULL;
    cJSON *j;
    if ((j = cJSON_GetObjectItem(pr, "orderID")) && cJSON_IsString(j)) oid = j->valuestring;
    else if ((j = cJSON_GetObjectItem(pr, "id")) && cJSON_IsString(j))  oid = j->valuestring;

    if (oid && g_n_placed_ids < MAX_PLACED_IDS) {
        strncpy(g_placed_ids[g_n_placed_ids++], oid, 63);
        printf("[poly] placed order %s\n", oid);
    }
    cJSON_Delete(pr);
}

/* ── Cancel all resting Poly orders ──────────────────────────────────────── */

static void cancel_all_poly_orders(CURL *curl)
{
    if (g_n_placed_ids == 0) return;
    printf("[poly] cancelling %d resting orders\n", g_n_placed_ids);

    /* Build cancel-all body: {"orderIDs": [...]} */
    cJSON *arr = cJSON_CreateArray();
    for (int i = 0; i < g_n_placed_ids; i++)
        cJSON_AddItemToArray(arr, cJSON_CreateString(g_placed_ids[i]));
    cJSON *body_obj = cJSON_CreateObject();
    cJSON_AddItemToObject(body_obj, "orderIDs", arr);
    char *body = cJSON_PrintUnformatted(body_obj);
    cJSON_Delete(body_obj);

    struct curl_slist *hdrs = l2_headers("DELETE", "/orders", body);
    char *resp = http_req(curl, "DELETE", CLOB_BASE "/orders", body, hdrs);
    curl_slist_free_all(hdrs);
    free(body);
    if (resp) {
        printf("[poly] cancel response: %.200s\n", resp);
        free(resp);
    }
}

/* ── Fill handler ─────────────────────────────────────────────────────────── */

/*
 * Binary fill handler.
 *
 * Decomposes the fill amount into its binary representation and submits
 * the pre-signed order for each set bit (slot i covers 2^i contracts).
 * Each submitted slot is immediately re-signed for the next fill.
 *
 * Example: fill=37 (binary 100101)
 *   → submit slots 0 (1 contract), 2 (4 contracts), 5 (32 contracts)
 *   → re-sign slots 0, 2, 5
 *   → total covered = 37 contracts, 3 orders placed
 */
static void handle_fill(CURL *curl, double filled_count, int is_bid)
{
    int          n_bits = is_bid ? g_bid_n_bits : g_ask_n_bits;
    SignedOrder *pool   = is_bid ? g_bid_orders : g_ask_orders;
    double       price  = is_bid ? g_resign_bid_price : g_resign_ask_price;

    if (n_bits == 0) {
        fprintf(stderr, "[poly] handle_fill: no binary slots signed for %s side\n",
                is_bid ? "bid" : "ask");
        return;
    }

    uint32_t fill = (uint32_t)(filled_count + 0.5);  /* round to nearest int */
    uint32_t max_cover = (n_bits < 32) ? ((1u << n_bits) - 1u) : 0xFFFFFFFFu;

    if (fill > max_cover) {
        fprintf(stderr, "[poly] fill %u exceeds binary coverage %u — capping\n",
                fill, max_cover);
        fill = max_cover;
    }

    int covered = 0;
    int orders_placed = 0;

    for (int i = 0; i < n_bits; i++) {
        if (fill & (1u << i)) {
            place_signed_order(curl, pool[i].body);
            orders_placed++;
            covered += (1 << i);
            /* Re-sign this slot immediately so it is ready for the next fill */
            sign_binary_slot(i, price, SIDE_BUY, g_resign_neg_risk,
                             g_resign_token_id, &pool[i]);
        }
    }

    printf("[poly] binary fill: requested=%u covered=%d orders_placed=%d\n",
           (unsigned)(filled_count + 0.5), covered, orders_placed);

    /*
     * Update Polymarket portfolio counters.
     *   Bid-pool orders -> NO exposure (hedging Kalshi YES buy)
     *   Ask-pool orders -> YES exposure (hedging Kalshi YES sell)
     */
    if (is_bid)
        g_no_held  += covered;
    else
        g_yes_held += covered;

    printf("[poly] portfolio: yes=%d no=%d\n", g_yes_held, g_no_held);

    /* Merge (net out) any offsetting YES+NO pairs */
    if (g_yes_held > 0 && g_no_held > 0) {
        int to_merge = g_yes_held < g_no_held ? g_yes_held : g_no_held;
        g_yes_held -= to_merge;
        g_no_held  -= to_merge;
        printf("[poly] merged %d contract pair(s); yes=%d no=%d remaining\n",
               to_merge, g_yes_held, g_no_held);
    }
}

/* ── poly_run: main state machine ─────────────────────────────────────────── */

void poly_run(const ArbConfig *cfg, const ArbCreds *creds,
              int fd_to_kalshi, int fd_from_kalshi)
{
    /* Set global credential pointers (used by sign_order, l2_headers) */
    g_poly_address = creds->poly_address;
    g_poly_api_key = creds->poly_api_key;
    g_poly_secret  = creds->poly_secret;
    g_poly_pass    = creds->poly_pass;
    g_eth_priv_key = creds->eth_priv_key;

    srand((unsigned)time(NULL) ^ (unsigned)getpid());

    signal(SIGTERM, poly_quit_handler);
    signal(SIGINT,  poly_quit_handler);

    /* ── libcurl ── */
    curl_global_init(CURL_GLOBAL_ALL);
    CURL *curl = curl_easy_init();
    if (!curl) { fprintf(stderr, "[poly] curl init failed\n"); return; }
    curl_easy_setopt(curl, CURLOPT_TCP_NODELAY,    1L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE,  1L);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);

    /* ── Connect WebSocket ── */
    printf("[poly] Connecting to Polymarket WS for token %s…\n",
           cfg->poly_token_id);
    struct lws_context *ctx = poly_ws_connect(cfg->poly_token_id);
    if (!ctx) goto cleanup;

    /* ── STATE: wait for initial book snapshot ── */
    printf("[poly] Waiting for orderbook snapshot…\n");
    {
        int64_t deadline = now_ms() + 30000;
        while (!g_ws.got_snapshot && !g_ws.done && now_ms() < deadline)
            lws_service(ctx, 10);
        if (!g_ws.got_snapshot) {
            fprintf(stderr, "[poly] timed out waiting for snapshot\n");
            goto cleanup;
        }
    }
    printf("[poly] Snapshot: bid=%.4f (vol=%.1f) ask=%.4f (vol=%.1f)\n",
           g_ws.best_bid, g_ws.best_bid_vol, g_ws.best_ask, g_ws.best_ask_vol);

    /* ── Send poly_book to Kalshi ── */
    {
        ArbMsg m = {0};
        m.type              = MSG_POLY_BOOK;
        m.d.poly_book.bid     = (double)g_ws.best_bid;
        m.d.poly_book.ask     = (double)g_ws.best_ask;
        m.d.poly_book.bid_vol = (double)g_ws.best_bid_vol;
        m.d.poly_book.ask_vol = (double)g_ws.best_ask_vol;
        if (!ipc_send(fd_to_kalshi, &m)) {
            fprintf(stderr, "[poly] failed to send poly_book\n");
            goto cleanup;
        }
        printf("[poly] Sent poly_book to Kalshi\n");
    }

    /* ── Wait for kalshi_signal ── */
    printf("[poly] Waiting for Kalshi arb signal…\n");
    ArbMsg sig_msg = {0};
    {
        int64_t deadline = now_ms() + 60000;
        int got_signal = 0;
        while (!got_signal && !g_ws.done && now_ms() < deadline) {
            lws_service(ctx, 10);
            int rc = ipc_recv_nb(fd_from_kalshi, &sig_msg);
            if (rc == 1 && sig_msg.type == MSG_KALSHI_SIGNAL) got_signal = 1;
            else if (rc == -1) { fprintf(stderr, "[poly] pipe EOF\n"); goto cleanup; }
        }
        if (!got_signal) {
            fprintf(stderr, "[poly] timed out waiting for kalshi signal\n");
            goto cleanup;
        }
    }

    int bid_ok = sig_msg.d.kalshi_signal.bid_ok;
    int ask_ok = sig_msg.d.kalshi_signal.ask_ok;
    if (!bid_ok && !ask_ok) {
        printf("[poly] No arb opportunity — aborting\n");
        goto cleanup;
    }

    int kalshi_bid = sig_msg.d.kalshi_signal.kalshi_bid;

    /* Track last bid/ask sent to Kalshi via MSG_POLY_PRICE_UPDATE.
     * Declared before redo_signing: so the backward goto is legal;
     * values reset each time we re-enter the monitoring loop. */
    float last_poly_bid_sent = 0.0f;
    float last_poly_ask_sent = 0.0f;
    float last_bid_vol_sent  = 0.0f;  /* best_bid_vol at last vol notification */
    float last_ask_vol_sent  = 0.0f;
    double filtered_bid_vol  = 0.0;
    double filtered_ask_vol  = 0.0;
    int kalshi_ask = sig_msg.d.kalshi_signal.kalshi_ask;
    printf("[poly] Arb signal: bid_leg=%s ask_leg=%s kalshi_bid=%d kalshi_ask=%d\n",
           bid_ok ? "YES" : "no", ask_ok ? "YES" : "no", kalshi_bid, kalshi_ask);

redo_signing:
    /* Volume available at arb-profitable prices:
     *   Bid leg: buy Kalshi at k_bid, sell Poly at poly_bid.
     *     → count Poly bids above k_bid (buyers paying more than Kalshi's bid).
     *   Ask leg: sell Kalshi at k_ask, buy Poly at poly_ask.
     *     → count Poly asks below k_ask (sellers cheaper than Kalshi's ask). */
    {
        float k_bid_f = kalshi_bid / 100.0f;
        float k_ask_f = kalshi_ask / 100.0f;
        double sv_bid = 0.0, sv_ask = 0.0;
        int ii;
        for (ii = 0; ii < g_ws.ob.n_bids; ii++)
            if (g_ws.ob.bids[ii].price > k_bid_f)
                sv_bid += g_ws.ob.bids[ii].size;
        for (ii = 0; ii < g_ws.ob.n_asks; ii++)
            if (g_ws.ob.asks[ii].price < k_ask_f)
                sv_ask += g_ws.ob.asks[ii].size;
        filtered_bid_vol = bid_ok ? ((sv_bid > 0.0) ? sv_bid : (double)g_ws.best_bid_vol) : 0.0;
        filtered_ask_vol = ask_ok ? ((sv_ask > 0.0) ? sv_ask : (double)g_ws.best_ask_vol) : 0.0;
        printf("[poly] Filtered vol: bid_vol=%.1f ask_vol=%.1f ",
               filtered_bid_vol, filtered_ask_vol);
        printf("(poly bids >%d¢ / poly asks <%d¢)\n", kalshi_bid, kalshi_ask);
    }
    /* ── Pre-sign 20 orders ── */
    printf("[poly] Pre-signing orders (bid@%.4f, ask@%.4f)…\n",
           1.0 - kalshi_bid/100.0, 1.0 - kalshi_ask/100.0);
    presign_orders(cfg->poly_token_id, kalshi_bid, kalshi_ask,
                   filtered_bid_vol, filtered_ask_vol,
                   cfg->neg_risk);

    /* ── Fetch Poly balance for budget-scaling on Kalshi side ── */
    double poly_balance = fetch_poly_balance(curl);

    /* ── Send poly_signing_done ── */
    {
        ArbMsg m = {0};
        m.type                             = MSG_POLY_SIGNING_DONE;
        m.d.poly_signing_done.poly_bid_vol = filtered_bid_vol;
        m.d.poly_signing_done.poly_ask_vol = filtered_ask_vol;
        m.d.poly_signing_done.poly_balance = poly_balance;
        if (!ipc_send(fd_to_kalshi, &m)) {
            fprintf(stderr, "[poly] failed to send signing_done\n");
            goto cleanup;
        }
        printf("[poly] Sent signing_done (poly_balance=$%.2f)\n", poly_balance);
    }

    /* Snapshot prices and volumes so we detect changes in the monitoring loop */
    last_poly_bid_sent = g_ws.best_bid;
    last_poly_ask_sent = g_ws.best_ask;
    last_bid_vol_sent  = g_ws.best_bid_vol;
    last_ask_vol_sent  = g_ws.best_ask_vol;

    /* ── Monitoring loop: fills / redo / abort ── */
    printf("[poly] Monitoring loop started\n");
    while (!g_ws.done) {
        lws_service(ctx, 10);

        if (g_poly_quit) {
            printf("[poly] Shutdown signal — aborting\n");
            ArbMsg am = {0};
            am.type = MSG_ABORT;
            am.d.abort_msg.reason = 3;
            ipc_send(fd_to_kalshi, &am);
            goto cleanup;
        }

        /* Send periodic PING to keep WS alive */
        if (now_ms() - g_ws.last_ping_ms > PING_INTERVAL_MS) {
            g_ws.send_ping = 1;
            lws_callback_on_writable(g_ws.wsi);
        }

        /* Send price update to Kalshi whenever best bid/ask changes */
        if (g_ws.best_bid != last_poly_bid_sent ||
            g_ws.best_ask != last_poly_ask_sent) {
            ArbMsg pu = {0};
            pu.type                    = MSG_POLY_PRICE_UPDATE;
            pu.d.poly_price_update.bid = (double)g_ws.best_bid;
            pu.d.poly_price_update.ask = (double)g_ws.best_ask;
            if (ipc_send(fd_to_kalshi, &pu)) {
                printf("[poly] Price update: bid=%.4f ask=%.4f\n",
                       g_ws.best_bid, g_ws.best_ask);
                last_poly_bid_sent = g_ws.best_bid;
                last_poly_ask_sent = g_ws.best_ask;
            }
        }
        /* Send vol update when best-vol drops >15% from last reported value */
        if ((bid_ok && g_ws.best_bid_vol < last_bid_vol_sent * 0.85f) ||
            (ask_ok && g_ws.best_ask_vol < last_ask_vol_sent * 0.85f)) {
            ArbMsg vu = {0};
            vu.type = MSG_POLY_VOL_UPDATE;
            vu.d.poly_vol_update.bid_vol = (double)g_ws.best_bid_vol;
            vu.d.poly_vol_update.ask_vol = (double)g_ws.best_ask_vol;
            if (ipc_send(fd_to_kalshi, &vu)) {
                printf("[poly] Vol drop >15%%: bid %.0f->%.0f ask %.0f->%.0f\n",
                       (double)last_bid_vol_sent, (double)g_ws.best_bid_vol,
                       (double)last_ask_vol_sent, (double)g_ws.best_ask_vol);
                last_bid_vol_sent = g_ws.best_bid_vol;
                last_ask_vol_sent = g_ws.best_ask_vol;
            }
        }

        /* Abort if price exceeded threshold */
        if (g_ws.abort_triggered) {
            printf("[poly] Price threshold exceeded — aborting\n");
            ArbMsg am = {0};
            am.type           = MSG_ABORT;
            am.d.abort_msg.reason = 1;
            ipc_send(fd_to_kalshi, &am);
            break;
        }

        /* Poll pipe */
        ArbMsg pipe_msg = {0};
        int rc = ipc_recv_nb(fd_from_kalshi, &pipe_msg);
        if (rc == -1) {
            fprintf(stderr, "[poly] pipe EOF from Kalshi\n");
            break;
        }
        if (rc == 0) continue;

        switch (pipe_msg.type) {
        case MSG_POLY_REDO_SIGNING:
            kalshi_bid = pipe_msg.d.redo_signing.kalshi_bid;
            kalshi_ask = pipe_msg.d.redo_signing.kalshi_ask;
            printf("[poly] Redo signing: bid=%d ask=%d\n", kalshi_bid, kalshi_ask);
            goto redo_signing;

        case MSG_KALSHI_FILL:
            printf("[poly] Kalshi fill: %.1f contracts (%s order)\n",
                   pipe_msg.d.kalshi_fill.filled_count,
                   pipe_msg.d.kalshi_fill.is_bid ? "bid" : "ask");
            handle_fill(curl, pipe_msg.d.kalshi_fill.filled_count,
                        pipe_msg.d.kalshi_fill.is_bid);
            break;

        case MSG_ABORT:
            printf("[poly] Abort received from Kalshi (reason=%d)\n",
                   pipe_msg.d.abort_msg.reason);
            goto abort_and_clean;

        default:
            break;
        }
    }

abort_and_clean:
    cancel_all_poly_orders(curl);

cleanup:
    if (ctx) lws_context_destroy(ctx);
    curl_easy_cleanup(curl);
    curl_global_cleanup();
    printf("[poly] Process exiting\n");
}
