/*
 * Polymarket live: CLOB REST (EIP-712 signed order, balance) + WebSocket (market orderbook).
 */
#define _POSIX_C_SOURCE 200809L
#define OPENSSL_API_COMPAT 10100
#include "poly_live.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <openssl/evp.h>
#include <openssl/hmac.h>
#include <openssl/ec.h>
#include <openssl/ecdsa.h>
#include <openssl/bn.h>
#include <openssl/bio.h>
#include <openssl/buffer.h>
#include <curl/curl.h>
#include <libwebsockets.h>
#include <cjson/cJSON.h>

#define CLOB_BASE   "https://clob.polymarket.com"
#define DATA_BASE   "https://data-api.polymarket.com"
#define WS_HOST     "ws-subscriptions-clob.polymarket.com"
#define WS_PATH     "/ws/market"
#define WS_PORT     443
#define CHAIN_ID    137u
#define CTF_NEG     "0xC5d563A36AE78145C45a50134d48A1215220f80a"
#define CTF         "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
#define FEE_BPS     "0"
#define SIG_TYPE    1u
#define KECCAK_RATE 136
#define P_MAX_LVL   64

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
        for (int i = 0; i < 5; i++) bc[i] = s[i]^s[i+5]^s[i+10]^s[i+15]^s[i+20];
        for (int i = 0; i < 5; i++) {
            t = bc[(i+4)%5] ^ ROT64(bc[(i+1)%5], 1);
            for (int j = 0; j < 25; j += 5) s[j+i] ^= t;
        }
        t = s[1];
        for (int i = 0; i < 24; i++) { int j = kpi[i]; bc[0]=s[j]; s[j]=ROT64(t,kro[i]); t=bc[0]; }
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
    pad[len] = 0x01;
    pad[KECCAK_RATE-1] |= 0x80;
    for (int i = 0; i < KECCAK_RATE; i++) b[i] ^= pad[i];
    keccakf(st);
    memcpy(out, st, 32);
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

static void abi_dec(uint8_t buf[32], const char *dec) {
    BIGNUM *bn = BN_new();
    BN_dec2bn(&bn, dec);
    memset(buf, 0, 32);
    BN_bn2binpad(bn, buf, 32);
    BN_free(bn);
}
static void abi_u64(uint8_t buf[32], uint64_t v) {
    memset(buf, 0, 32);
    for (int i = 7; i >= 0; i--) { buf[24+i] = (uint8_t)(v & 0xFF); v >>= 8; }
}
static void abi_addr(uint8_t buf[32], const char *hex) {
    BIGNUM *bn = BN_new();
    const char *p = (strncmp(hex, "0x", 2) == 0) ? hex+2 : hex;
    BN_hex2bn(&bn, p);
    memset(buf, 0, 32);
    BN_bn2binpad(bn, buf, 32);
    BN_free(bn);
}
static void abi_str(uint8_t buf[32], const char *str) {
    keccak256((const uint8_t *)str, strlen(str), buf);
}

static void bytes_to_hex(const uint8_t *s, size_t n, char *d) {
    for (size_t i = 0; i < n; i++) snprintf(d + i*2, 3, "%02x", s[i]);
}

static int eth_sign(const uint8_t digest[32], const char *priv_hex, uint8_t r[32], uint8_t s[32], uint8_t *v) {
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
    const BIGNUM *rb, *sb;
    ECDSA_SIG_get0(sig, &rb, &sb);
    BN_bn2binpad(rb, r, 32);
    BN_bn2binpad(sb, s, 32);
    BN_CTX *ctx = BN_CTX_new();
    BIGNUM *order = BN_new();
    EC_GROUP_get_order(grp, order, ctx);
    *v = 27;
    for (int recid = 0; recid <= 1; recid++) {
        BIGNUM *rx = BN_dup(rb);
        EC_POINT *R = EC_POINT_new(grp);
        if (EC_POINT_set_compressed_coordinates(grp, R, rx, recid & 1, ctx)) {
            BIGNUM *h = BN_bin2bn(digest, 32, NULL);
            BIGNUM *rinv = BN_new();
            BN_mod_inverse(rinv, rb, order, ctx);
            BIGNUM *u1 = BN_new();
            BN_zero(u1);
            BIGNUM *tmp = BN_new();
            BN_mod_mul(tmp, h, rinv, order, ctx);
            BN_mod_sub(u1, u1, tmp, order, ctx);
            BIGNUM *u2 = BN_new();
            BN_mod_mul(u2, sb, rinv, order, ctx);
            EC_POINT *Q = EC_POINT_new(grp);
            EC_POINT_mul(grp, Q, u1, R, u2, ctx);
            if (EC_POINT_cmp(grp, Q, pub, ctx) == 0) {
                *v = (uint8_t)(27 + recid);
                EC_POINT_free(Q); BN_free(h); BN_free(rinv); BN_free(u1); BN_free(tmp); BN_free(u2);
                EC_POINT_free(R); BN_free(rx);
                break;
            }
            EC_POINT_free(Q); BN_free(h); BN_free(rinv); BN_free(u1); BN_free(tmp); BN_free(u2);
        }
        EC_POINT_free(R); BN_free(rx);
    }
    BN_free(order);
    BN_CTX_free(ctx);
    ECDSA_SIG_free(sig);
    EC_KEY_free(key);
    BN_free(priv);
    EC_POINT_free(pub);
    return 1;
}

static void sign_order_f(const char *token_id, const char *maker, const char *salt,
                         const char *maker_amt, const char *taker_amt, int side, int neg_risk,
                         const char *eth_priv, char sig_hex[135]) {
    static const char d[] = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
    static const char o[] = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";
    uint8_t d_hash[32], o_hash[32];
    keccak256((const uint8_t *)d, strlen(d), d_hash);
    keccak256((const uint8_t *)o, strlen(o), o_hash);
    const char *ex = neg_risk ? CTF_NEG : CTF;
    uint8_t dom_enc[5*32], *dp = dom_enc;
    memcpy(dp, d_hash, 32); dp += 32;
    abi_str(dp, "CTF Exchange"); dp += 32;
    abi_str(dp, "1"); dp += 32;
    abi_u64(dp, CHAIN_ID); dp += 32;
    abi_addr(dp, ex); dp += 32;
    uint8_t dom_sep[32];
    keccak256(dom_enc, sizeof(dom_enc), dom_sep);
    uint8_t struct_enc[13*32], *sp = struct_enc;
    memcpy(sp, o_hash, 32); sp += 32;
    abi_dec(sp, salt); sp += 32;
    abi_addr(sp, maker); sp += 32;
    abi_addr(sp, maker); sp += 32;
    abi_addr(sp, "0x0000000000000000000000000000000000000000"); sp += 32;
    abi_dec(sp, token_id); sp += 32;
    abi_dec(sp, maker_amt); sp += 32;
    abi_dec(sp, taker_amt); sp += 32;
    abi_u64(sp, 0); abi_u64(sp, 0); sp += 64;
    abi_dec(sp, FEE_BPS); sp += 32;
    abi_u64(sp, (uint64_t)side); sp += 32;
    abi_u64(sp, SIG_TYPE); sp += 32;
    uint8_t struct_hash[32];
    keccak256(struct_enc, sizeof(struct_enc), struct_hash);
    uint8_t pre[66];
    pre[0] = 0x19; pre[1] = 0x01;
    memcpy(pre+2, dom_sep, 32);
    memcpy(pre+34, struct_hash, 32);
    uint8_t digest[32];
    keccak256(pre, 66, digest);
    uint8_t r[32], s[32], v;
    if (!eth_sign(digest, eth_priv, r, s, &v)) { strcpy(sig_hex, "0x"); return; }
    sig_hex[0] = '0'; sig_hex[1] = 'x';
    bytes_to_hex(r, 32, sig_hex+2);
    bytes_to_hex(s, 32, sig_hex+66);
    snprintf(sig_hex+130, 5, "%02x", v);
}

typedef struct { char *buf; size_t len; } RBuf;
static size_t wcb(void *d, size_t sz, size_t n, void *u) {
    RBuf *r = (RBuf *)u;
    size_t t = sz * n;
    r->buf = realloc(r->buf, r->len + t + 1);
    memcpy(r->buf + r->len, d, t);
    r->len += t;
    r->buf[r->len] = '\0';
    return t;
}

struct PolyLive {
    char address[256];
    char api_key[256];
    char secret[512];
    char pass[256];
    char eth_priv[256];
    char token_id[256];
    int neg_risk;
    CURL *curl;
    CURL *data_curl;  /* reused for Data API GETs to avoid init/cleanup churn and fragmentation */
    struct lws_context *ctx;
    struct lws *wsi;
    int got_snapshot;
    int done;
    double bids_p[P_MAX_LVL], bids_s[P_MAX_LVL];
    double asks_p[P_MAX_LVL], asks_s[P_MAX_LVL];
    int n_bids, n_asks;
};

static char *hmac_sig(const char *secret_b64, const char *ts, const char *method, const char *path, const char *body) {
    uint8_t key[128];
    size_t klen = b64_decode(secret_b64, key, sizeof(key));
    char msg[4096];
    snprintf(msg, sizeof(msg), "%s%s%s%s", ts, method, path, body ? body : "");
    uint8_t digest[32];
    unsigned int dlen = 32;
    HMAC(EVP_sha256(), key, (int)klen, (const uint8_t *)msg, strlen(msg), digest, &dlen);
    BIO *b64 = BIO_new(BIO_f_base64());
    BIO *mem = BIO_new(BIO_s_mem());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    BIO_push(b64, mem);
    BIO_write(b64, digest, 32);
    BIO_flush(b64);
    BUF_MEM *bm;
    BIO_get_mem_ptr(mem, &bm);
    char *out = malloc(bm->length + 1);
    memcpy(out, bm->data, bm->length);
    out[bm->length] = '\0';
    BIO_free_all(b64);
    return out;
}

static struct curl_slist *l2_headers(PolyLive *p, const char *method, const char *path, const char *body) {
    char ts[32];
    snprintf(ts, sizeof(ts), "%lld", (long long)(arb_now_ms() / 1000));
    char *sig = hmac_sig(p->secret, ts, method, path, body);
    struct curl_slist *sl = NULL;
    char h[512];
    snprintf(h, sizeof(h), "POLY_ADDRESS: %s", p->address); sl = curl_slist_append(sl, h);
    snprintf(h, sizeof(h), "POLY_SIGNATURE: %s", sig); sl = curl_slist_append(sl, h);
    snprintf(h, sizeof(h), "POLY_TIMESTAMP: %s", ts); sl = curl_slist_append(sl, h);
    snprintf(h, sizeof(h), "POLY_API_KEY: %s", p->api_key); sl = curl_slist_append(sl, h);
    snprintf(h, sizeof(h), "POLY_PASSPHRASE: %s", p->pass); sl = curl_slist_append(sl, h);
    free(sig);
    sl = curl_slist_append(sl, "Content-Type: application/json");
    return sl;
}

/* Optional out params: http_status_out (e.g. 200, 401), curl_ok_out (1 if perform succeeded). */
static char *http_req_ex(PolyLive *p, const char *method, const char *url, const char *body,
                         long *http_status_out, int *curl_ok_out) {
    const char *path = strstr(url, "clob.polymarket.com") ? strchr(url + 8, '/') : "/";
    if (!path) path = "/";
    struct curl_slist *hdrs = l2_headers(p, method, path, body);
    RBuf r = { NULL, 0 };
    curl_easy_setopt(p->curl, CURLOPT_URL, url);
    curl_easy_setopt(p->curl, CURLOPT_WRITEFUNCTION, wcb);
    curl_easy_setopt(p->curl, CURLOPT_WRITEDATA, &r);
    curl_easy_setopt(p->curl, CURLOPT_HTTPHEADER, hdrs);
    curl_easy_setopt(p->curl, CURLOPT_FOLLOWLOCATION, 1L);
    if (strcmp(method, "POST") == 0) {
        curl_easy_setopt(p->curl, CURLOPT_POST, 1L);
        curl_easy_setopt(p->curl, CURLOPT_POSTFIELDS, body ? body : "");
    } else {
        curl_easy_setopt(p->curl, CURLOPT_HTTPGET, 1L);
    }
    CURLcode cres = curl_easy_perform(p->curl);
    if (curl_ok_out) *curl_ok_out = (cres == CURLE_OK) ? 1 : 0;
    if (cres != CURLE_OK) {
        fprintf(stderr, "[poly] HTTP %s %s curl error: %s\n",
                method, url, curl_easy_strerror(cres));
    }
    if (http_status_out) {
        long code = 0;
        curl_easy_getinfo(p->curl, CURLINFO_RESPONSE_CODE, &code);
        *http_status_out = code;
        if (code >= 400) {
            fprintf(stderr, "[poly] HTTP %s %s status %ld\n", method, url, code);
        }
    }
    curl_slist_free_all(hdrs);
    curl_easy_setopt(p->curl, CURLOPT_POST, 0L);
    curl_easy_setopt(p->curl, CURLOPT_POSTFIELDS, NULL);
    return r.buf ? r.buf : strdup("");
}

static char *http_req(PolyLive *p, const char *method, const char *url, const char *body) {
    return http_req_ex(p, method, url, body, NULL, NULL);
}

/* Unauthenticated GET for Data API (positions, etc.) - no L2 headers. Reuses p->data_curl to avoid per-call init/cleanup. */
static char *http_req_data_get(PolyLive *p, const char *url) {
    CURL *curl = p->data_curl;
    if (!curl) return strdup("");
    curl_easy_reset(curl);
    RBuf r = { NULL, 0 };
    struct curl_slist *hdrs = NULL;
    hdrs = curl_slist_append(hdrs, "Accept: application/json");
    curl_easy_setopt(curl, CURLOPT_URL, url);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, wcb);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &r);
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, hdrs);
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
    curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
    curl_easy_perform(curl);
    curl_slist_free_all(hdrs);
    return r.buf ? r.buf : strdup("");
}

static PolyLive *g_poly_ws = NULL;

static int poly_ws_cb(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in, size_t len) {
    PolyLive *p = g_poly_ws;
    if (!p) return 0;
    (void)user;
    switch (reason) {
    case LWS_CALLBACK_CLIENT_ESTABLISHED: {
        p->wsi = wsi;
        char sub[512];
        snprintf(sub, sizeof(sub), "{\"assets_ids\":[\"%s\"],\"type\":\"market\",\"custom_feature_enabled\":true}", p->token_id);
        size_t sl = strlen(sub);
        uint8_t *buf = malloc(LWS_PRE + sl);
        memcpy(buf + LWS_PRE, sub, sl);
        lws_write(wsi, buf + LWS_PRE, sl, LWS_WRITE_TEXT);
        free(buf);
        break;
    }
    case LWS_CALLBACK_CLIENT_RECEIVE: {
        if (len == 4 && strncmp((char *)in, "PONG", 4) == 0) break;
        cJSON *root = cJSON_ParseWithLength((char *)in, len);
        if (!root) {
            fprintf(stderr, "[poly] WS parse error (len=%zu)\n", len);
            break;
        }
        cJSON *arr = cJSON_IsArray(root) ? root : NULL;
        int alen = arr ? cJSON_GetArraySize(arr) : 1;
        for (int mi = 0; mi < alen; mi++) {
            cJSON *m = arr ? cJSON_GetArrayItem(arr, mi) : root;
            if (!m) continue;
            if (cJSON_GetObjectItem(m, "bids") || cJSON_GetObjectItem(m, "asks")) {
                p->n_bids = 0;
                p->n_asks = 0;
                cJSON *bids = cJSON_GetObjectItem(m, "bids");
                if (cJSON_IsArray(bids)) {
                    int nb = cJSON_GetArraySize(bids);
                    for (int i = 0; i < nb && i < P_MAX_LVL; i++) {
                        cJSON *b = cJSON_GetArrayItem(bids, i);
                        const char *pr = cJSON_GetStringValue(cJSON_GetObjectItem(b, "price"));
                        const char *sz = cJSON_GetStringValue(cJSON_GetObjectItem(b, "size"));
                        p->bids_p[p->n_bids] = pr ? atof(pr) : 0;
                        p->bids_s[p->n_bids] = sz ? atof(sz) : 0;
                        p->n_bids++;
                    }
                }
                cJSON *asks = cJSON_GetObjectItem(m, "asks");
                if (cJSON_IsArray(asks)) {
                    int na = cJSON_GetArraySize(asks);
                    for (int i = 0; i < na && i < P_MAX_LVL; i++) {
                        cJSON *a = cJSON_GetArrayItem(asks, i);
                        const char *pr = cJSON_GetStringValue(cJSON_GetObjectItem(a, "price"));
                        const char *sz = cJSON_GetStringValue(cJSON_GetObjectItem(a, "size"));
                        p->asks_p[p->n_asks] = pr ? atof(pr) : 0;
                        p->asks_s[p->n_asks] = sz ? atof(sz) : 0;
                        p->n_asks++;
                    }
                }
                p->got_snapshot = 1;
                printf("[poly] orderbook snapshot: n_bids=%d n_asks=%d\n",
                       p->n_bids, p->n_asks);
            } else if (strcmp(cJSON_GetStringValue(cJSON_GetObjectItem(m, "type")) != NULL ? cJSON_GetStringValue(cJSON_GetObjectItem(m, "type")) : "", "price_change") == 0) {
                cJSON *ch = cJSON_GetObjectItem(m, "changes");
                if (cJSON_IsArray(ch)) {
                    int nc = cJSON_GetArraySize(ch);
                    for (int i = 0; i < nc; i++) {
                        cJSON *c = cJSON_GetArrayItem(ch, i);
                        const char *pr_s = cJSON_GetStringValue(cJSON_GetObjectItem(c, "price"));
                        const char *sz_s = cJSON_GetStringValue(cJSON_GetObjectItem(c, "size"));
                        const char *side_s = cJSON_GetStringValue(cJSON_GetObjectItem(c, "side"));
                        double pr = atof(pr_s ? pr_s : "0");
                        double sz = atof(sz_s ? sz_s : "0");
                        int is_bid = (strcmp(side_s ? side_s : "", "BUY") == 0);
                        int found = 0;
                        if (is_bid) {
                            for (int j = 0; j < p->n_bids; j++) {
                                if (p->bids_p[j] == pr) {
                                    if (sz <= 0) {
                                        memmove(&p->bids_p[j], &p->bids_p[j+1], (p->n_bids - j - 1) * sizeof(double));
                                        memmove(&p->bids_s[j], &p->bids_s[j+1], (p->n_bids - j - 1) * sizeof(double));
                                        p->n_bids--;
                                    } else { p->bids_s[j] = sz; }
                                    found = 1;
                                    break;
                                }
                            }
                            if (!found && sz > 0 && p->n_bids < P_MAX_LVL) {
                                p->bids_p[p->n_bids] = pr;
                                p->bids_s[p->n_bids] = sz;
                                p->n_bids++;
                            }
                            printf("[poly] delta: side=BUY price=%.4f size=%.4f n_bids=%d\n",
                                   pr, sz, p->n_bids);
                        } else {
                            for (int j = 0; j < p->n_asks; j++) {
                                if (p->asks_p[j] == pr) {
                                    if (sz <= 0) {
                                        memmove(&p->asks_p[j], &p->asks_p[j+1], (p->n_asks - j - 1) * sizeof(double));
                                        memmove(&p->asks_s[j], &p->asks_s[j+1], (p->n_asks - j - 1) * sizeof(double));
                                        p->n_asks--;
                                    } else { p->asks_s[j] = sz; }
                                    found = 1;
                                    break;
                                }
                            }
                            if (!found && sz > 0 && p->n_asks < P_MAX_LVL) {
                                p->asks_p[p->n_asks] = pr;
                                p->asks_s[p->n_asks] = sz;
                                p->n_asks++;
                            }
                            printf("[poly] delta: side=SELL price=%.4f size=%.4f n_asks=%d\n",
                                   pr, sz, p->n_asks);
                        }
                    }
                }
            }
        }
        cJSON_Delete(root);
        break;
    }
    case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
    case LWS_CALLBACK_CLIENT_CLOSED:
        p->done = 1;
        break;
    default:
        break;
    }
    return 0;
}

static struct lws_protocols poly_protos[] = {
    { "poly-market", poly_ws_cb, 0, 262144, 0, NULL, 0 },
    LWS_PROTOCOL_LIST_TERM
};

PolyLive *poly_live_create(const ArbCreds *creds, const char *token_id, int neg_risk) {
    if (!creds || !token_id) return NULL;
    PolyLive *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    size_t len = strlen(creds->poly_address);
    if (len >= sizeof(p->address)) len = sizeof(p->address) - 1;
    memcpy(p->address, creds->poly_address, len); p->address[len] = '\0';
    len = strlen(creds->poly_api_key);
    if (len >= sizeof(p->api_key)) len = sizeof(p->api_key) - 1;
    memcpy(p->api_key, creds->poly_api_key, len); p->api_key[len] = '\0';
    len = strlen(creds->poly_secret);
    if (len >= sizeof(p->secret)) len = sizeof(p->secret) - 1;
    memcpy(p->secret, creds->poly_secret, len); p->secret[len] = '\0';
    len = strlen(creds->poly_pass);
    if (len >= sizeof(p->pass)) len = sizeof(p->pass) - 1;
    memcpy(p->pass, creds->poly_pass, len); p->pass[len] = '\0';
    len = strlen(creds->eth_priv_key);
    if (len >= sizeof(p->eth_priv)) len = sizeof(p->eth_priv) - 1;
    memcpy(p->eth_priv, creds->eth_priv_key, len); p->eth_priv[len] = '\0';
    len = strlen(token_id);
    if (len >= sizeof(p->token_id)) len = sizeof(p->token_id) - 1;
    memcpy(p->token_id, token_id, len); p->token_id[len] = '\0';
    p->neg_risk = neg_risk ? 1 : 0;
    p->curl = curl_easy_init();
    if (!p->curl) { free(p); return NULL; }
    p->data_curl = curl_easy_init();
    if (!p->data_curl) { curl_easy_cleanup(p->curl); free(p); return NULL; }
    return p;
}

void poly_live_destroy(PolyLive *p) {
    if (!p) return;
    if (p->ctx) lws_context_destroy(p->ctx);
    if (p->data_curl) curl_easy_cleanup(p->data_curl);
    curl_easy_cleanup(p->curl);
    free(p);
}

static void build_order_json(const char *salt, const char *maker, const char *token_id,
                             const char *maker_amt, const char *taker_amt, int side, const char *sig,
                             char *out, size_t outsz) {
    cJSON *order = cJSON_CreateObject();
    cJSON_AddStringToObject(order, "salt", salt);
    cJSON_AddStringToObject(order, "maker", maker);
    cJSON_AddStringToObject(order, "signer", maker);
    cJSON_AddStringToObject(order, "taker", "0x0000000000000000000000000000000000000000");
    cJSON_AddStringToObject(order, "tokenId", token_id);
    cJSON_AddStringToObject(order, "makerAmount", maker_amt);
    cJSON_AddStringToObject(order, "takerAmount", taker_amt);
    cJSON_AddStringToObject(order, "expiration", "0");
    cJSON_AddStringToObject(order, "nonce", "0");
    cJSON_AddStringToObject(order, "feeRateBps", FEE_BPS);
    cJSON_AddNumberToObject(order, "side", side);
    cJSON_AddNumberToObject(order, "signatureType", (int)SIG_TYPE);
    cJSON_AddStringToObject(order, "signature", sig);
    cJSON *body = cJSON_CreateObject();
    cJSON_AddItemToObject(body, "order", order);
    cJSON_AddStringToObject(body, "owner", maker);
    cJSON_AddStringToObject(body, "orderType", "GTC");
    char *s = cJSON_PrintUnformatted(body);
    strncpy(out, s, outsz - 1);
    out[outsz - 1] = '\0';
    free(s);
    cJSON_Delete(body);
}

int poly_live_build_signed_order(PolyLive *p, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size) {
    return poly_live_build_signed_buy_order(p, p->token_id, price_0_1, size_outcome_tokens, out, out_size);
}

/* Build a signed BUY order for the given token (token_id = token to buy). */
int poly_live_build_signed_buy_order(PolyLive *p, const char *token_id, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size) {
    if (!token_id || !token_id[0]) return 0;
    uint64_t taker_amt = size_outcome_tokens * 1000000ULL;
    uint64_t maker_amt = (uint64_t)(price_0_1 * (double)size_outcome_tokens * 1e6);
    char taker_s[32], maker_s[32], salt[32], sig_hex[135];
    snprintf(taker_s, sizeof(taker_s), "%llu", (unsigned long long)taker_amt);
    snprintf(maker_s, sizeof(maker_s), "%llu", (unsigned long long)maker_amt);
    snprintf(salt, sizeof(salt), "%llu", (unsigned long long)arb_now_ms());
    sign_order_f(token_id, p->address, salt, maker_s, taker_s, 0, p->neg_risk, p->eth_priv, sig_hex);
    build_order_json(salt, p->address, token_id, maker_s, taker_s, 0, sig_hex, out, out_size);
    return 1;
}

/* Build a signed SELL order for the given token (e.g. opposite token for sell-first hedge). */
int poly_live_build_signed_sell_order(PolyLive *p, const char *token_id, double price_0_1, uint64_t size_outcome_tokens, char *out, size_t out_size) {
    if (!token_id || !token_id[0]) return 0;
    uint64_t maker_amt = size_outcome_tokens * 1000000ULL;  /* maker gives tokens */
    uint64_t taker_amt = (uint64_t)(price_0_1 * (double)size_outcome_tokens * 1e6);  /* taker gives USDC */
    char taker_s[32], maker_s[32], salt[32], sig_hex[135];
    snprintf(maker_s, sizeof(maker_s), "%llu", (unsigned long long)maker_amt);
    snprintf(taker_s, sizeof(taker_s), "%llu", (unsigned long long)taker_amt);
    snprintf(salt, sizeof(salt), "%llu", (unsigned long long)arb_now_ms());
    sign_order_f(token_id, p->address, salt, maker_s, taker_s, 1, p->neg_risk, p->eth_priv, sig_hex);
    build_order_json(salt, p->address, token_id, maker_s, taker_s, 1, sig_hex, out, out_size);
    return 1;
}

int poly_live_place_order(PolyLive *p, const char *order_body_json) {
    return poly_live_place_order_attempt(p, order_body_json) == POLY_PLACE_OK ? 1 : 0;
}

int poly_live_place_order_attempt(PolyLive *p, const char *order_body_json) {
    char url[256];
    snprintf(url, sizeof(url), "%s/order", CLOB_BASE);
    long http_status = 0;
    int curl_ok = 0;
    char *resp = http_req_ex(p, "POST", url, order_body_json, &http_status, &curl_ok);
    if (!curl_ok) {
        if (resp) free(resp);
        return POLY_PLACE_ERR_NETWORK;
    }
    if (http_status == 401 || http_status == 403) {
        if (resp) free(resp);
        return POLY_PLACE_ERR_AUTH;
    }
    if (http_status == 429) {
        if (resp) free(resp);
        return POLY_PLACE_ERR_RATE;
    }
    int ok = 0;
    if (resp) {
        cJSON *root = cJSON_Parse(resp);
        if (root && (cJSON_GetObjectItem(root, "orderID") || cJSON_GetObjectItem(root, "id")))
            ok = 1;
        if (root) cJSON_Delete(root);
        free(resp);
    }
    if (ok) return POLY_PLACE_OK;
    return POLY_PLACE_ERR_OTHER;
}

/*
 * Get position size (in outcome tokens) for a given token_id.
 * Uses Data API GET /positions?user=... (no auth). Returns 0 if not found or error.
 */
double poly_live_get_position(PolyLive *p, const char *token_id) {
    if (!p || !token_id || !token_id[0]) return 0.0;
    char url[512];
    snprintf(url, sizeof(url), "%s/positions?user=%s&sizeThreshold=0&limit=500", DATA_BASE, p->address);
    char *resp = http_req_data_get(p, url);
    double size = 0.0;
    if (resp) {
        cJSON *root = cJSON_Parse(resp);
        free(resp);
        if (root && cJSON_IsArray(root)) {
            int n = cJSON_GetArraySize(root);
            for (int i = 0; i < n; i++) {
                cJSON *pos = cJSON_GetArrayItem(root, i);
                if (!pos) continue;
                cJSON *asset = cJSON_GetObjectItem(pos, "asset");
                const char *astr = cJSON_IsString(asset) ? asset->valuestring : NULL;
                if (astr && strcmp(astr, token_id) == 0) {
                    cJSON *sz = cJSON_GetObjectItem(pos, "size");
                    if (cJSON_IsNumber(sz))
                        size = sz->valuedouble;
                    break;
                }
            }
        }
        if (root) cJSON_Delete(root);
    }
    return size;
}

double poly_live_get_balance(PolyLive *p) {
    char *resp = http_req(p, "GET", CLOB_BASE "/accounts", "");
    double bal = 0.0;
    if (resp) {
        cJSON *root = cJSON_Parse(resp);
        free(resp);
        if (root) {
            cJSON *obj = cJSON_IsArray(root) ? cJSON_GetArrayItem(root, 0) : root;
            if (obj) {
                cJSON *b = cJSON_GetObjectItem(obj, "balance");
                if (b) bal = cJSON_IsString(b) ? atof(b->valuestring) : b->valuedouble;
            }
            cJSON_Delete(root);
        }
    }
    return bal;
}

int poly_live_ws_connect(PolyLive *p) {
    g_poly_ws = p;
    struct lws_context_creation_info ci = { 0 };
    ci.port = CONTEXT_PORT_NO_LISTEN;
    ci.protocols = poly_protos;
    ci.options = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT | LWS_SERVER_OPTION_DISABLE_IPV6;
    ci.ssl_ca_filepath = "/etc/pki/tls/certs/ca-bundle.crt";
    lws_set_log_level(LLL_ERR | LLL_WARN, NULL);
    p->ctx = lws_create_context(&ci);
    if (!p->ctx) return 0;
    struct lws_client_connect_info cc = { 0 };
    cc.context = p->ctx;
    cc.address = WS_HOST;
    cc.port = WS_PORT;
    cc.path = WS_PATH;
    cc.host = WS_HOST;
    cc.origin = WS_HOST;
    cc.ssl_connection = LCCSCF_USE_SSL;
    cc.protocol = poly_protos[0].name;
    if (!lws_client_connect_via_info(&cc)) {
        lws_context_destroy(p->ctx);
        p->ctx = NULL;
        return 0;
    }
    int64_t deadline = arb_now_ms() + 15000;
    while (!p->got_snapshot && !p->done && arb_now_ms() < deadline) {
        lws_service(p->ctx, 50);
    }
    return p->got_snapshot ? 1 : 0;
}

void poly_live_ws_service(PolyLive *p, int timeout_ms) {
    if (p->ctx) lws_service(p->ctx, timeout_ms);
}

int poly_live_ws_got_orderbook(PolyLive *p) {
    return p->got_snapshot;
}

void poly_live_ws_copy_orderbook(PolyLive *p, PolyFullBookPayload *out) {
    memset(out, 0, sizeof(*out));
    out->n_bids = (uint16_t)(p->n_bids > ARB_MAX_LEVELS ? ARB_MAX_LEVELS : p->n_bids);
    out->n_asks = (uint16_t)(p->n_asks > ARB_MAX_LEVELS ? ARB_MAX_LEVELS : p->n_asks);
    for (uint16_t i = 0; i < out->n_bids; i++) {
        out->bids[i].price = p->bids_p[i];
        out->bids[i].size = p->bids_s[i];
    }
    for (uint16_t i = 0; i < out->n_asks; i++) {
        out->asks[i].price = p->asks_p[i];
        out->asks[i].size = p->asks_s[i];
    }
}

int poly_live_ws_done(PolyLive *p) {
    return p->done;
}
