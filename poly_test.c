/*
 * polymarket_ws.c
 *
 * Polymarket equivalent of nyc_weather_ws.c.
 * Uses the Polymarket WebSocket market channel (no auth) for a live orderbook
 * snapshot and the CLOB REST API to place and cancel a small limit order.
 * Measures and prints the round-trip latency for each operation.
 *
 * Flow:
 *   1. REST  GET  gamma-api/markets?keyword=New+York+temperature  → token_id
 *   2. WSS   /ws/market subscribe with token_id  → receive "book" snapshot  (timed)
 *   3. REST  POST clob/order                     → place EIP-712 signed order (timed)
 *   4. REST  DELETE clob/orders/{id}             → cancel order               (timed)
 *   5. Print timing summary
 *
 * Authentication:
 *   Market WS channel  – no auth
 *   Order placement    – EIP-712 signed order body (secp256k1 + keccak-256)
 *                        + HMAC-SHA256 L2 request headers
 *   Order cancellation – HMAC-SHA256 L2 request headers only
 *
 * Dependencies:
 *   libcurl       – REST HTTP requests
 *   libwebsockets – WebSocket client (v4.x)
 *   cjson         – JSON parsing / building
 *   openssl       – HMAC-SHA256, secp256k1 ECDSA, base64
 *
 * Build (Linux / macOS):
 *   gcc -O2 -Wall -Wextra -o polymarket_ws polymarket_ws.c \
 *       -lcurl -lwebsockets -lcjson -lssl -lcrypto -lpthread
 *
 * Build (Windows, MSYS2 MinGW64):
 *   gcc -O2 -Wall -Wextra -o polymarket_ws.exe polymarket_ws.c \
 *       -lcurl -lwebsockets -lcjson -lssl -lcrypto -lpthread -lws2_32
 *
 * Credentials (fill in your own before building):
 *   POLY_ADDRESS   – your Polymarket proxy wallet address (from polymarket.com/settings)
 *   POLY_API_KEY   – L2 API key  (derive with py-clob-client or SDK)
 *   POLY_SECRET    – L2 API secret (base64-encoded)
 *   POLY_PASS      – L2 API passphrase
 *   ETH_PRIV_KEY   – hex private key of the SIGNING wallet (no "0x" prefix)
 *                    For POLY_PROXY accounts this is the key exported from
 *                    polymarket.com/settings → "Export private key"
 */

 #include <stdio.h>
 #include <stdlib.h>
 #include <string.h>
 #include <stdint.h>
 #include <time.h>
 
 #ifdef _WIN32
 #  include <winsock2.h>
 #  include <windows.h>
    typedef LARGE_INTEGER hr_time_t;
    static void    hr_now(hr_time_t *t) { QueryPerformanceCounter(t); }
    static double  hr_ms(const hr_time_t *a, const hr_time_t *b) {
        LARGE_INTEGER f; QueryPerformanceFrequency(&f);
        return (double)(b->QuadPart - a->QuadPart) * 1000.0 / f.QuadPart;
    }
    static int64_t now_ms(void) {
        FILETIME ft; GetSystemTimeAsFileTime(&ft);
        ULARGE_INTEGER u; u.LowPart = ft.dwLowDateTime; u.HighPart = ft.dwHighDateTime;
        return (int64_t)((u.QuadPart - 116444736000000000ULL) / 10000);
    }
 #else
 #  include <unistd.h>
    typedef struct timespec hr_time_t;
    static void    hr_now(hr_time_t *t) { clock_gettime(CLOCK_MONOTONIC, t); }
    static double  hr_ms(const hr_time_t *a, const hr_time_t *b) {
        return (b->tv_sec  - a->tv_sec ) * 1000.0
             + (b->tv_nsec - a->tv_nsec) / 1e6;
    }
    static int64_t now_ms(void) {
        struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
        return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
    }
 #endif
 
 #include <curl/curl.h>
 #include <libwebsockets.h>
 #include <cjson/cJSON.h>
 
 #include <openssl/evp.h>
 #include <openssl/hmac.h>
 #include <openssl/ec.h>
 #include <openssl/ecdsa.h>
 #include <openssl/obj_mac.h>   /* NID_secp256k1 */
 #include <openssl/bn.h>
 #include <openssl/bio.h>
 #include <openssl/buffer.h>
 
 /* ── Credentials  ────────────────────────────────────────────────────────── */
 /* Replace with your own.  Never commit real keys to version control.        */
 
/* Read from environment variables (set via: source .env or export in shell)  */
static const char *POLY_ADDRESS  = NULL;
static const char *POLY_API_KEY  = NULL;
static const char *POLY_SECRET   = NULL;
static const char *POLY_PASS     = NULL;
static const char *ETH_PRIV_KEY  = NULL;
 
 /* ── Configuration ───────────────────────────────────────────────────────── */
 
 #define GAMMA_BASE   "https://gamma-api.polymarket.com"
 #define CLOB_BASE    "https://clob.polymarket.com"
 #define WS_HOST      "ws-subscriptions-clob.polymarket.com"
 #define WS_PATH      "/ws/market"
 #define WS_PORT      443
 
 /* CTF Exchange contract on Polygon mainnet (non-neg-risk markets) */
 #define CTF_EXCHANGE "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
 #define CHAIN_ID     137   /* Polygon mainnet */
 
 /* Signature type: 0=EOA, 1=POLY_PROXY, 2=GNOSIS_SAFE */
 #define SIG_TYPE     1
 
 /* Order: buy 1 outcome token at $0.01 (GTC limit, almost certainly won't fill) */
 #define ORDER_MAKER_AMOUNT  "10000"    /* 0.01 USDC  (6 decimals) */
 #define ORDER_TAKER_AMOUNT  "1000000" /* 1 token    (6 decimals) */
 #define ORDER_SIDE          0          /* 0 = BUY                  */
 #define ORDER_FEE_RATE_BPS  "0"
 
 /* How many WS delta messages to capture before closing */
 #define DELTA_CAP 5
 
 /* ── Compact Keccak-256 ──────────────────────────────────────────────────── */
 /* Ethereum uses keccak-256, which differs from SHA3-256 in the padding byte  */
 
 #define KECCAK_RATE 136   /* rate for 256-bit capacity: (1600-512)/8 bytes    */
 
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
     pad[len]            = 0x01; /* keccak padding — 0x06 for SHA3 */
     pad[KECCAK_RATE-1] |= 0x80;
     for (int i = 0; i < KECCAK_RATE; i++) b[i] ^= pad[i];
     keccakf(st);
     memcpy(out, st, 32);
 }
 
 /* ── Base64 helpers ──────────────────────────────────────────────────────── */
 
 /* Encode `len` bytes to a null-terminated base64 string (caller frees). */
 static char *b64_encode(const uint8_t *data, size_t len) {
     BIO *b64 = BIO_new(BIO_f_base64());
     BIO *mem = BIO_new(BIO_s_mem());
     BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
     BIO_push(b64, mem);
     BIO_write(b64, data, (int)len);
     BIO_flush(b64);
     BUF_MEM *bm;
     BIO_get_mem_ptr(mem, &bm);
     char *out = (char *)malloc(bm->length + 1);
     memcpy(out, bm->data, bm->length);
     out[bm->length] = '\0';
     BIO_free_all(b64);
     return out;
 }
 
 /* Decode base64 string; writes up to *out_len bytes. Returns bytes written. */
 static size_t b64_decode(const char *in, uint8_t *out, size_t max) {
     BIO *b64 = BIO_new(BIO_f_base64());
     BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
     BIO *mem = BIO_new_mem_buf(in, -1);
     BIO_push(b64, mem);
     int n = BIO_read(b64, out, (int)max);
     BIO_free_all(b64);
     return (n > 0) ? (size_t)n : 0;
 }
 
 /* Encode 32 bytes to lowercase hex (65-char null-terminated buffer). */
 static void bytes_to_hex(const uint8_t *src, size_t len, char *dst) {
     for (size_t i = 0; i < len; i++)
         snprintf(dst + i*2, 3, "%02x", src[i]);
 }
 
 /* ── HMAC-SHA256 L2 authentication ──────────────────────────────────────── */
 /*
  * L2 signature = Base64(HMAC-SHA256(Base64Decode(secret), message))
  * message      = timestamp + METHOD + request_path + body
  */
 static char *hmac_l2_signature(const char *secret_b64,
                                 const char *timestamp,
                                 const char *method,
                                 const char *path,
                                 const char *body) {
     /* Decode the base64 API secret */
     uint8_t key[128];
     size_t  key_len = b64_decode(secret_b64, key, sizeof(key));
 
     /* Build message: timestamp + METHOD + path + body */
     char msg[4096];
     snprintf(msg, sizeof(msg), "%s%s%s%s", timestamp, method, path,
              body ? body : "");
 
     uint8_t digest[32];
     unsigned int dlen = 32;
     HMAC(EVP_sha256(), key, (int)key_len,
          (const uint8_t *)msg, strlen(msg), digest, &dlen);
 
     return b64_encode(digest, dlen);
 }
 
 /* Build all five L2 headers into a curl slist. Caller frees the list. */
 static struct curl_slist *l2_headers(const char *method,
                                      const char *path,
                                      const char *body) {
     char ts[32];
     snprintf(ts, sizeof(ts), "%lld", (long long)now_ms() / 1000);
 
     char *sig = hmac_l2_signature(POLY_SECRET, ts, method, path, body);
 
     char hdr[512];
     struct curl_slist *sl = NULL;
 
     snprintf(hdr, sizeof(hdr), "POLY_ADDRESS: %s",   POLY_ADDRESS);  sl = curl_slist_append(sl, hdr);
     snprintf(hdr, sizeof(hdr), "POLY_SIGNATURE: %s", sig);           sl = curl_slist_append(sl, hdr);
     snprintf(hdr, sizeof(hdr), "POLY_TIMESTAMP: %s", ts);            sl = curl_slist_append(sl, hdr);
     snprintf(hdr, sizeof(hdr), "POLY_API_KEY: %s",   POLY_API_KEY);  sl = curl_slist_append(sl, hdr);
     snprintf(hdr, sizeof(hdr), "POLY_PASSPHRASE: %s",POLY_PASS);     sl = curl_slist_append(sl, hdr);
 
     free(sig);
     sl = curl_slist_append(sl, "Content-Type: application/json");
     return sl;
 }
 
 /* ── EIP-712 ABI helpers ─────────────────────────────────────────────────── */
 /*
  * All fields in an EIP-712 struct are ABI-encoded as 32-byte big-endian words.
  * Strings and bytes are replaced by their keccak256 hash.
  */
 
 /* Write a decimal string as uint256 big-endian into buf[32]. */
 static void abi_dec(uint8_t buf[32], const char *dec) {
     BIGNUM *bn = BN_new();
     BN_dec2bn(&bn, dec);
     memset(buf, 0, 32);
     BN_bn2binpad(bn, buf, 32);
     BN_free(bn);
 }
 
 /* Write a uint64 as uint256 big-endian into buf[32]. */
 static void abi_u64(uint8_t buf[32], uint64_t v) {
     memset(buf, 0, 32);
     for (int i = 7; i >= 0; i--) { buf[24+i] = (uint8_t)(v & 0xFF); v >>= 8; }
 }
 
 /* Write an 0x-prefixed hex address as a 32-byte ABI word (12 zero bytes + 20). */
 static void abi_addr(uint8_t buf[32], const char *hex) {
     BIGNUM *bn = BN_new();
     const char *p = (strncmp(hex, "0x", 2) == 0) ? hex + 2 : hex;
     BN_hex2bn(&bn, p);
     memset(buf, 0, 32);
     BN_bn2binpad(bn, buf, 32);
     BN_free(bn);
 }
 
 /* Hash a UTF-8 string with keccak256 and write the 32-byte result. */
 static void abi_str(uint8_t buf[32], const char *str) {
     keccak256((const uint8_t *)str, strlen(str), buf);
 }
 
 /* ── secp256k1 ECDSA signing with recovery bit ───────────────────────────── */
 /*
  * Signs `digest` with the private key given as 64 hex chars (no "0x").
  * Outputs r[32], s[32], and v (27 or 28) for Ethereum compatibility.
  * Returns 1 on success.
  */
 static int eth_sign(const uint8_t digest[32], const char *priv_hex,
                     uint8_t r_out[32], uint8_t s_out[32], uint8_t *v_out) {
     EC_KEY *key = EC_KEY_new_by_curve_name(NID_secp256k1);
 
     BIGNUM *priv = BN_new();
     const char *p = (strncmp(priv_hex, "0x", 2) == 0) ? priv_hex + 2 : priv_hex;
     BN_hex2bn(&priv, p);
     EC_KEY_set_private_key(key, priv);
 
     /* Derive and set public key */
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
 
     /* Find the recovery id (0 or 1): try each and check which recovers pub. */
     BN_CTX *ctx = BN_CTX_new();
     BIGNUM *order = BN_new();
     EC_GROUP_get_order(grp, order, ctx);
 
     *v_out = 27; /* fallback; correctly overwritten below */
     for (int recid = 0; recid <= 1; recid++) {
         BIGNUM *rx = BN_dup(r);
         EC_POINT *R = EC_POINT_new(grp);
         if (EC_POINT_set_compressed_coordinates(grp, R, rx, recid & 1, ctx)) {
             BIGNUM *h    = BN_bin2bn(digest, 32, NULL);
             BIGNUM *rinv = BN_new(); BN_mod_inverse(rinv, r, order, ctx);
             /* u1 = -hash * r^-1 mod order  (safe negate via mod_sub) */
             BIGNUM *u1   = BN_new(); BN_zero(u1);
             BIGNUM *tmp  = BN_new(); BN_mod_mul(tmp, h, rinv, order, ctx);
             BN_mod_sub(u1, u1, tmp, order, ctx);
             BIGNUM *u2   = BN_new(); BN_mod_mul(u2, s, rinv, order, ctx);
             EC_POINT *Q  = EC_POINT_new(grp);
             EC_POINT_mul(grp, Q, u1, R, u2, ctx);
 
             if (EC_POINT_cmp(grp, Q, pub, ctx) == 0) {
                 *v_out = (uint8_t)(27 + recid);
                 EC_POINT_free(Q);
                 BN_free(h); BN_free(rinv); BN_free(u1); BN_free(tmp); BN_free(u2);
                 EC_POINT_free(R); BN_free(rx);
                 break;
             }
             EC_POINT_free(Q);
             BN_free(h); BN_free(rinv); BN_free(u1); BN_free(tmp); BN_free(u2);
         }
         EC_POINT_free(R);
         BN_free(rx);
     }
 
     BN_free(order); BN_CTX_free(ctx);
     ECDSA_SIG_free(sig);
     EC_KEY_free(key); BN_free(priv); EC_POINT_free(pub);
     return 1;
 }
 
 /* ── EIP-712 Order signing ───────────────────────────────────────────────── */
 /*
  * Builds the 65-byte Ethereum signature (r+s+v) for a Polymarket limit order.
  * The result is written to `sig_hex` as "0x" + 130 hex chars.
  *
  * All numeric parameters are decimal strings (token_id can be a huge uint256).
  */
 static void sign_order(const char *token_id,
                        const char *maker,
                        const char *salt,
                        const char *maker_amount,
                        const char *taker_amount,
                        int         side,
                        char        sig_hex[135]) {
     /* ── 1. Type hashes ── */
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
 
     /* ── 2. Domain separator ── */
     /* encode: domainTypeHash || keccak(name) || keccak(version)
      *         || chainId || verifyingContract */
     uint8_t domain_enc[5*32];
     uint8_t *dp = domain_enc;
 
     memcpy(dp, domain_type_hash, 32);             dp += 32;
     abi_str(dp, "CTF Exchange");                   dp += 32;
     abi_str(dp, "1");                              dp += 32;
     abi_u64(dp, CHAIN_ID);                         dp += 32;
     abi_addr(dp, CTF_EXCHANGE);                    dp += 32;
 
     uint8_t domain_sep[32];
     keccak256(domain_enc, sizeof(domain_enc), domain_sep);
 
     /* ── 3. Struct hash ── */
     /* 13 fields (typeHash + 12 order fields), each 32 bytes */
     uint8_t struct_enc[13*32];
     uint8_t *sp = struct_enc;
 
     memcpy(sp, order_type_hash, 32);               sp += 32;
     abi_dec(sp, salt);                             sp += 32; /* salt          */
     abi_addr(sp, maker);                           sp += 32; /* maker         */
     abi_addr(sp, maker);                           sp += 32; /* signer = maker */
     /* taker = zero address */
     abi_addr(sp, "0x0000000000000000000000000000000000000000"); sp += 32;
     abi_dec(sp, token_id);                         sp += 32; /* tokenId       */
     abi_dec(sp, maker_amount);                     sp += 32; /* makerAmount   */
     abi_dec(sp, taker_amount);                     sp += 32; /* takerAmount   */
     abi_u64(sp, 0);                                sp += 32; /* expiration    */
     abi_u64(sp, 0);                                sp += 32; /* nonce         */
     abi_dec(sp, ORDER_FEE_RATE_BPS);               sp += 32; /* feeRateBps    */
     abi_u64(sp, (uint64_t)side);                   sp += 32; /* side          */
     abi_u64(sp, SIG_TYPE);                         sp += 32; /* signatureType */
 
     uint8_t struct_hash[32];
     keccak256(struct_enc, sizeof(struct_enc), struct_hash);
 
     /* ── 4. Final digest: 0x19 0x01 || domainSep || structHash ── */
     uint8_t pre[66];
     pre[0] = 0x19; pre[1] = 0x01;
     memcpy(pre + 2,  domain_sep,  32);
     memcpy(pre + 34, struct_hash, 32);
 
     uint8_t digest[32];
     keccak256(pre, 66, digest);
 
     /* ── 5. Sign ── */
     uint8_t r[32], s[32], v;
     if (!eth_sign(digest, ETH_PRIV_KEY, r, s, &v)) {
         fprintf(stderr, "eth_sign failed\n");
         strcpy(sig_hex, "0x");
         return;
     }
 
     /* Format as "0x" + r(64) + s(64) + v(2) = 132 chars */
     sig_hex[0] = '0'; sig_hex[1] = 'x';
     bytes_to_hex(r, 32, sig_hex + 2);
     bytes_to_hex(s, 32, sig_hex + 66);
     snprintf(sig_hex + 130, 5, "%02x", v);
 }
 
 /* ── HTTP response buffer ────────────────────────────────────────────────── */
 
 typedef struct { char *buf; size_t len; } RespBuf;
 
 static size_t write_cb(void *data, size_t sz, size_t nmemb, void *userp) {
     RespBuf *r = (RespBuf *)userp;
     size_t total = sz * nmemb;
     r->buf = (char *)realloc(r->buf, r->len + total + 1);
     memcpy(r->buf + r->len, data, total);
     r->len += total;
     r->buf[r->len] = '\0';
     return total;
 }
 
 /* ── REST helper ─────────────────────────────────────────────────────────── */
 /*
  * method : "GET", "POST", "DELETE"
  * url    : full URL
  * body   : JSON body string (or NULL)
  * hdrs   : extra curl_slist headers (or NULL)
  * Returns allocated response string (caller frees), or NULL on error.
  */
 static char *http_req(CURL *curl, const char *method, const char *url,
                       const char *body, struct curl_slist *hdrs) {
     RespBuf resp = {NULL, 0};
 
     curl_easy_setopt(curl, CURLOPT_URL,           url);
     curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION,  write_cb);
     curl_easy_setopt(curl, CURLOPT_WRITEDATA,      &resp);
     curl_easy_setopt(curl, CURLOPT_HTTPHEADER,     hdrs);
     curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
 
     if (strcmp(method, "GET") == 0) {
         curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
     } else if (strcmp(method, "POST") == 0) {
         curl_easy_setopt(curl, CURLOPT_POST,           1L);
         curl_easy_setopt(curl, CURLOPT_POSTFIELDS,     body ? body : "");
         curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE,  (long)(body ? strlen(body) : 0));
     } else if (strcmp(method, "DELETE") == 0) {
         curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, "DELETE");
         if (body) {
             curl_easy_setopt(curl, CURLOPT_POSTFIELDS,    body);
             curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE, (long)strlen(body));
         } else {
             curl_easy_setopt(curl, CURLOPT_POSTFIELDS,    "");
             curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE, 0L);
         }
     }
 
     CURLcode rc = curl_easy_perform(curl);
     if (rc != CURLE_OK) {
         fprintf(stderr, "curl error: %s\n", curl_easy_strerror(rc));
         free(resp.buf);
         return NULL;
     }
     /* Reset custom request for next call */
     curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, NULL);
     return resp.buf ? resp.buf : strdup("");
 }
 
 /* ── WebSocket state ─────────────────────────────────────────────────────── */
 
 #define MAX_DELTAS 16
 
 typedef struct {
     char     token_id[128];        /* subscribed asset id          */
     int      got_book;             /* received initial book snapshot */
     int      done;                 /* set to 1 when ready to exit  */
 
     /* Orderbook top-of-book from snapshot */
     char     best_bid_price[32];
     char     best_bid_size[32];
     char     best_ask_price[32];
     char     best_ask_size[32];
 
     /* Timing */
     hr_time_t t_connect;           /* when we initiated the connection */
     hr_time_t t_book;              /* when we received the book message */
 
     /* Ping / pong RTT */
     int       ping_sent;
     int64_t   ping_ms;             /* wall-clock ms when PING was sent */
     double    ping_rtt_ms;         /* PING → PONG round-trip           */
 
     /* price_change (delta) inter-arrival */
     int       delta_count;
     double    delta_ms[MAX_DELTAS];
     hr_time_t t_last_delta;
     double    book_to_first_delta_ms; /* time from book to first delta  */
 } WsState;
 
 /* ── WebSocket callback ──────────────────────────────────────────────────── */
 
 static int ws_cb(struct lws *wsi, enum lws_callback_reasons reason,
                  void *user, void *in, size_t len) {
     WsState *st = (WsState *)lws_wsi_user(wsi);
     (void)user;
 
     switch (reason) {
 
     case LWS_CALLBACK_CLIENT_ESTABLISHED: {
         /* Send subscription message immediately */
         char sub[256];
         snprintf(sub, sizeof(sub),
                  "{\"assets_ids\":[\"%s\"],\"type\":\"market\"}", st->token_id);
         size_t slen = strlen(sub);
         uint8_t *msg = (uint8_t *)malloc(LWS_PRE + slen);
         memcpy(msg + LWS_PRE, sub, slen);
         lws_write(wsi, msg + LWS_PRE, slen, LWS_WRITE_TEXT);
         free(msg);
 
         /* Schedule a ping after the snapshot */
         lws_callback_on_writable(wsi);
         break;
     }
 
     case LWS_CALLBACK_CLIENT_RECEIVE: {
         const char *data = (const char *)in;
 
         /* Heartbeat PONG */
         if (len == 4 && strncmp(data, "PONG", 4) == 0) {
             if (st->ping_sent) {
                 int64_t now = now_ms();
                 st->ping_rtt_ms = (double)(now - st->ping_ms);
                 st->ping_sent   = 0;
             }
             break;
         }
 
         cJSON *msg = cJSON_ParseWithLength(data, len);
         if (!msg) break;
 
         cJSON *type_j = cJSON_GetObjectItem(msg, "type");
         const char *mtype = type_j ? type_j->valuestring : "";
 
         if (strcmp(mtype, "book") == 0 && !st->got_book) {
             hr_now(&st->t_book);
             st->got_book = 1;
 
             /* Extract best bid / ask */
             cJSON *bids = cJSON_GetObjectItem(msg, "bids");
             cJSON *asks = cJSON_GetObjectItem(msg, "asks");
             if (bids && cJSON_GetArraySize(bids) > 0) {
                 /* Bids are highest-first */
                 cJSON *b = cJSON_GetArrayItem(bids, 0);
                 cJSON *pr = cJSON_GetObjectItem(b, "price");
                 cJSON *sz = cJSON_GetObjectItem(b, "size");
                 if (pr) strncpy(st->best_bid_price, pr->valuestring,
                                 sizeof(st->best_bid_price)-1);
                 if (sz) strncpy(st->best_bid_size,  sz->valuestring,
                                 sizeof(st->best_bid_size)-1);
             }
             if (asks && cJSON_GetArraySize(asks) > 0) {
                 /* Asks are lowest-first */
                 cJSON *a = cJSON_GetArrayItem(asks, 0);
                 cJSON *pr = cJSON_GetObjectItem(a, "price");
                 cJSON *sz = cJSON_GetObjectItem(a, "size");
                 if (pr) strncpy(st->best_ask_price, pr->valuestring,
                                 sizeof(st->best_ask_price)-1);
                 if (sz) strncpy(st->best_ask_size,  sz->valuestring,
                                 sizeof(st->best_ask_size)-1);
             }
 
             /* Send a PING to measure network RTT */
             lws_callback_on_writable(wsi);
 
         } else if (strcmp(mtype, "price_change") == 0 && st->got_book) {
             hr_time_t now;
             hr_now(&now);
 
             if (st->delta_count == 0)
                 st->book_to_first_delta_ms = hr_ms(&st->t_book, &now);
 
             if (st->delta_count < MAX_DELTAS) {
                 st->delta_ms[st->delta_count] =
                     (st->delta_count == 0)
                     ? st->book_to_first_delta_ms
                     : hr_ms(&st->t_last_delta, &now);
                 st->delta_count++;
             }
             st->t_last_delta = now;
 
             if (st->delta_count >= DELTA_CAP && st->ping_rtt_ms > 0)
                 st->done = 1;
         }
 
         cJSON_Delete(msg);
         break;
     }
 
     case LWS_CALLBACK_CLIENT_WRITEABLE: {
         /* Send PING (text message, not WS ping frame) */
         if (st->got_book && !st->ping_sent) {
             const char *ping = "PING";
             size_t plen = 4;
             uint8_t *buf = (uint8_t *)malloc(LWS_PRE + plen);
             memcpy(buf + LWS_PRE, ping, plen);
             lws_write(wsi, buf + LWS_PRE, plen, LWS_WRITE_TEXT);
             free(buf);
             st->ping_ms   = now_ms();
             st->ping_sent = 1;
         }
         break;
     }
 
     case LWS_CALLBACK_CLIENT_CONNECTION_ERROR:
         fprintf(stderr, "WS connection error: %s\n",
                 in ? (char *)in : "(unknown)");
         if (st) st->done = 1;
         break;
 
     case LWS_CALLBACK_CLIENT_CLOSED:
         if (st) st->done = 1;
         break;
 
     default:
         break;
     }
     return 0;
 }
 
 /* ── WebSocket orderbook fetch ───────────────────────────────────────────── */
 
 static struct lws_protocols ws_protocols[] = {
     {"polymarket-market", ws_cb, 0, 65536, 0, NULL, 0},
     LWS_PROTOCOL_LIST_TERM
 };
 
 static int fetch_orderbook_ws(const char *token_id, WsState *st) {
     memset(st, 0, sizeof(*st));
     strncpy(st->token_id, token_id, sizeof(st->token_id)-1);
     st->token_id[sizeof(st->token_id)-1] = '\0';
 
     struct lws_context_creation_info ctx_info;
    memset(&ctx_info, 0, sizeof(ctx_info));
    ctx_info.port      = CONTEXT_PORT_NO_LISTEN;
    ctx_info.protocols = ws_protocols;
    ctx_info.options   = LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT;
    ctx_info.options  |= LWS_SERVER_OPTION_DISABLE_IPV6;
    ctx_info.gid       = -1;
    ctx_info.uid       = -1;
    /* Use system CA bundle for SSL cert verification */
    ctx_info.ssl_ca_filepath = "/etc/pki/tls/certs/ca-bundle.crt";

    lws_set_log_level(LLL_ERR | LLL_WARN | LLL_NOTICE, NULL);

    struct lws_context *ctx = lws_create_context(&ctx_info);
    if (!ctx) { fprintf(stderr, "lws_create_context failed\n"); return 0; }

    struct lws_client_connect_info ci;
    memset(&ci, 0, sizeof(ci));
    ci.context        = ctx;
    ci.address        = WS_HOST;
    ci.port           = WS_PORT;
    ci.path           = WS_PATH;
    ci.host           = WS_HOST;
    ci.origin         = WS_HOST;
    ci.protocol       = NULL;  /* let server negotiate subprotocol */
    ci.ssl_connection = LCCSCF_USE_SSL;
    ci.userdata       = st;

    hr_now(&st->t_connect);
    struct lws *wsi = lws_client_connect_via_info(&ci);
    if (!wsi) {
        fprintf(stderr, "[ws] lws_client_connect_via_info returned NULL\n");
        lws_context_destroy(ctx);
        return 0;
    }
 
     /* Service loop: run until done or timeout (10 s) */
     int64_t deadline = now_ms() + 10000;
     while (!st->done && now_ms() < deadline)
         lws_service(ctx, 50);
 
     lws_context_destroy(ctx);
     return st->got_book;
 }
 
 /* ── Gamma API: find NYC temperature market ──────────────────────────────── */
 
 static int find_nyc_market(CURL *curl, char *token_id_out, size_t tid_size,
                             char *question_out, size_t q_size) {
     const char *url =
         GAMMA_BASE "/markets?keyword=New+York+temperature"
         "&active=true&closed=false&limit=10";
 
     struct curl_slist *hdrs = curl_slist_append(NULL, "Accept: application/json");
     char *resp = http_req(curl, "GET", url, NULL, hdrs);
     curl_slist_free_all(hdrs);
     if (!resp) return 0;
 
     cJSON *root = cJSON_Parse(resp);
     free(resp);
     if (!root) { fprintf(stderr, "Failed to parse Gamma response\n"); return 0; }
 
     /* Response is a JSON array of markets */
     int found = 0;
     int n = cJSON_GetArraySize(root);
     for (int i = 0; i < n && !found; i++) {
         cJSON *m = cJSON_GetArrayItem(root, i);
 
         /* Get the YES token id from clobTokenIds[0] */
         cJSON *tokens = cJSON_GetObjectItem(m, "clobTokenIds");
         if (!tokens) continue;
 
         /* clobTokenIds may be a JSON string (array of strings encoded as
          * a JSON string itself) or an actual array — handle both */
         const char *first_token = NULL;
         char token_buf[128] = {0};
 
         if (cJSON_IsArray(tokens) && cJSON_GetArraySize(tokens) > 0) {
             cJSON *t0 = cJSON_GetArrayItem(tokens, 0);
             if (cJSON_IsString(t0)) first_token = t0->valuestring;
         } else if (cJSON_IsString(tokens)) {
             /* Sometimes encoded as "[\"id1\",\"id2\"]" */
             cJSON *inner = cJSON_Parse(tokens->valuestring);
             if (inner && cJSON_IsArray(inner) && cJSON_GetArraySize(inner) > 0) {
                 cJSON *t0 = cJSON_GetArrayItem(inner, 0);
                 if (cJSON_IsString(t0)) {
                     strncpy(token_buf, t0->valuestring, sizeof(token_buf)-1);
                     first_token = token_buf;
                 }
             }
             cJSON_Delete(inner);
         }
 
        if (!first_token || strlen(first_token) == 0) continue;

        /* Filter: question must mention temperature/high temp and New York/NYC */
        cJSON *q = cJSON_GetObjectItem(m, "question");
        if (!q || !cJSON_IsString(q)) continue;
        const char *qstr = q->valuestring;
        int has_temp = (strstr(qstr, "temp") || strstr(qstr, "Temp") ||
                        strstr(qstr, "high") || strstr(qstr, "High"));
        int has_nyc  = (strstr(qstr, "New York") || strstr(qstr, "NYC") ||
                        strstr(qstr, "nyc"));
        if (!has_temp || !has_nyc) continue;

        /* Copy results */
        strncpy(token_id_out, first_token, tid_size-1);
        token_id_out[tid_size-1] = '\0';

        strncpy(question_out, qstr, q_size-1);
        question_out[q_size-1] = '\0';
        found = 1;
    }
 
     cJSON_Delete(root);
 
     if (!found)
         fprintf(stderr, "No active NYC temperature market found on Polymarket.\n");
 
     return found;
 }
 
 /* ── Display helpers ─────────────────────────────────────────────────────── */
 
 static void print_orderbook(const WsState *st) {
     printf("  Best bid : %s @ $%s\n", st->best_bid_size, st->best_bid_price);
     printf("  Best ask : %s @ $%s\n", st->best_ask_size, st->best_ask_price);
 }
 
 /* ── main ────────────────────────────────────────────────────────────────── */
 
int main(void) {
    POLY_ADDRESS = getenv("POLY_ADDRESS");
    POLY_API_KEY = getenv("POLY_API_KEY");
    POLY_SECRET  = getenv("POLY_SECRET");
    POLY_PASS    = getenv("POLY_PASSPHRASE");
    ETH_PRIV_KEY = getenv("ETH_PRIV_KEY");

    if (!POLY_API_KEY || !POLY_SECRET || !POLY_PASS) {
        fprintf(stderr, "Error: POLY_API_KEY, POLY_SECRET, and POLY_PASSPHRASE "
                        "must be set (source .env first)\n");
        return 1;
    }

    printf("============================================================\n");
    printf(" Polymarket – NYC weather market WS/REST benchmark\n");
    printf("============================================================\n\n");

    curl_global_init(CURL_GLOBAL_ALL);
     CURL *curl = curl_easy_init();
     if (!curl) { fprintf(stderr, "curl_easy_init failed\n"); return 1; }
 
     /* Suppress verbose output; follow redirects */
     curl_easy_setopt(curl, CURLOPT_VERBOSE, 0L);
     curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 1L);
 
     /* ── Step 1: find market ── */
     printf("[1] Finding active NYC temperature market on Polymarket…\n");
     char token_id[128] = {0};
     char question[256] = {0};
 
     if (!find_nyc_market(curl, token_id, sizeof(token_id),
                           question, sizeof(question))) {
         curl_easy_cleanup(curl);
         curl_global_cleanup();
         return 1;
     }
     printf("    Question : %s\n", question);
     printf("    Token ID : %s…\n\n", token_id[0] ? token_id : "(none)");
 
     /* ── Step 2: WebSocket orderbook snapshot ── */
     printf("[2] Fetching orderbook via WebSocket…\n");
     WsState ws;
     hr_time_t t0, t1;
 
     hr_now(&t0);
     int ok = fetch_orderbook_ws(token_id, &ws);
     hr_now(&t1);
     double t_orderbook = hr_ms(&t0, &t1);
 
     if (!ok) {
         fprintf(stderr, "    Failed to get orderbook snapshot.\n");
         curl_easy_cleanup(curl);
         curl_global_cleanup();
         return 1;
     }
 
     print_orderbook(&ws);
 
     printf("  Snapshot latency (incl. TLS) : %.1f ms\n", t_orderbook);
     if (ws.ping_rtt_ms > 0)
         printf("  Ping/pong RTT               : %.1f ms  (%.1f ms one-way est.)\n",
                ws.ping_rtt_ms, ws.ping_rtt_ms / 2.0);
     if (ws.delta_count > 0) {
         printf("  Book → first price_change   : %.1f ms\n",
                ws.book_to_first_delta_ms);
         printf("  price_change inter-arrivals :\n");
         for (int i = 0; i < ws.delta_count; i++)
             printf("    [%d] %7.1f ms%s\n", i+1, ws.delta_ms[i],
                    i == 0 ? "  ← book → first delta" : "");
     }
     printf("\n");
 
     /* ── Step 3: place order ── */
     printf("[3] Placing limit-buy order (1 token @ $0.01)…\n");
 
     /* Build EIP-712 signature */
     char sig_hex[135] = {0};
     char salt_buf[32];
     srand((unsigned int)time(NULL));
     snprintf(salt_buf, sizeof(salt_buf), "%u%u", (unsigned)rand(), (unsigned)rand());
 
     sign_order(token_id, POLY_ADDRESS, salt_buf,
                ORDER_MAKER_AMOUNT, ORDER_TAKER_AMOUNT, ORDER_SIDE, sig_hex);
 
     /* Build order JSON */
     cJSON *order_obj = cJSON_CreateObject();
     cJSON_AddStringToObject(order_obj, "salt",          salt_buf);
     cJSON_AddStringToObject(order_obj, "maker",         POLY_ADDRESS);
     cJSON_AddStringToObject(order_obj, "signer",        POLY_ADDRESS);
     cJSON_AddStringToObject(order_obj, "taker",
                             "0x0000000000000000000000000000000000000000");
     cJSON_AddStringToObject(order_obj, "tokenId",       token_id);
     cJSON_AddStringToObject(order_obj, "makerAmount",   ORDER_MAKER_AMOUNT);
     cJSON_AddStringToObject(order_obj, "takerAmount",   ORDER_TAKER_AMOUNT);
     cJSON_AddStringToObject(order_obj, "expiration",    "0");
     cJSON_AddStringToObject(order_obj, "nonce",         "0");
     cJSON_AddStringToObject(order_obj, "feeRateBps",    ORDER_FEE_RATE_BPS);
     cJSON_AddNumberToObject(order_obj, "side",          ORDER_SIDE);
     cJSON_AddNumberToObject(order_obj, "signatureType", SIG_TYPE);
     cJSON_AddStringToObject(order_obj, "signature",     sig_hex);
 
     cJSON *body_obj = cJSON_CreateObject();
     cJSON_AddItemToObject(body_obj, "order", order_obj);
     cJSON_AddStringToObject(body_obj, "owner",     POLY_ADDRESS);
     cJSON_AddStringToObject(body_obj, "orderType", "GTC");
 
     char *order_body = cJSON_PrintUnformatted(body_obj);
     cJSON_Delete(body_obj);
 
     struct curl_slist *place_hdrs = l2_headers("POST", "/order", order_body);
 
     hr_now(&t0);
     char *place_resp = http_req(curl, "POST", CLOB_BASE "/order",
                                 order_body, place_hdrs);
     hr_now(&t1);
     double t_place = hr_ms(&t0, &t1);
 
     curl_slist_free_all(place_hdrs);
     free(order_body);
 
     if (!place_resp) {
         fprintf(stderr, "    Place order request failed.\n");
         curl_easy_cleanup(curl); curl_global_cleanup(); return 1;
     }
 
     /* Extract order id */
     char order_id[128] = {0};
     cJSON *pr = cJSON_Parse(place_resp);
     if (pr) {
         cJSON *oid = cJSON_GetObjectItem(pr, "orderID");
         if (oid && cJSON_IsString(oid))
             strncpy(order_id, oid->valuestring, sizeof(order_id)-1);
         /* Some endpoints return "id" instead */
         if (!order_id[0]) {
             oid = cJSON_GetObjectItem(pr, "id");
             if (oid && cJSON_IsString(oid))
                 strncpy(order_id, oid->valuestring, sizeof(order_id)-1);
         }
         cJSON *succ = cJSON_GetObjectItem(pr, "success");
         printf("    Response : success=%s  orderID=%s\n",
                succ ? (cJSON_IsTrue(succ) ? "true" : "false") : "n/a",
                order_id[0] ? order_id : "(not returned)");
         cJSON_Delete(pr);
     } else {
         printf("    Raw response: %.200s\n", place_resp);
     }
     free(place_resp);
     printf("    Latency  : %.1f ms\n\n", t_place);
 
     /* ── Step 4: cancel order ── */
     double t_cancel = 0.0;
     if (order_id[0]) {
         printf("[4] Cancelling order %s…\n", order_id);
 
         char del_path[192];
         snprintf(del_path, sizeof(del_path), "/orders/%s", order_id);
         char del_url[256];
         snprintf(del_url,  sizeof(del_url),  "%s%s", CLOB_BASE, del_path);
 
         struct curl_slist *cancel_hdrs = l2_headers("DELETE", del_path, NULL);
 
         hr_now(&t0);
         char *cancel_resp = http_req(curl, "DELETE", del_url, NULL, cancel_hdrs);
         hr_now(&t1);
         t_cancel = hr_ms(&t0, &t1);
 
         curl_slist_free_all(cancel_hdrs);
 
         if (cancel_resp) {
             printf("    Response : %.200s\n", cancel_resp);
             free(cancel_resp);
         }
         printf("    Latency  : %.1f ms\n\n", t_cancel);
     } else {
         printf("[4] Skipping cancel (no order ID returned).\n\n");
     }
 
     /* ── Timing summary ── */
     printf("============================================================\n");
     printf(" TIMING SUMMARY\n");
     printf("============================================================\n");
     printf("  Orderbook snapshot (WSS)  : %8.1f ms  (incl. TLS handshake)\n",
            t_orderbook);
     if (ws.ping_rtt_ms > 0)
         printf("  Per-msg RTT (ping/pong)   : %8.1f ms  (%.1f ms one-way)\n",
                ws.ping_rtt_ms, ws.ping_rtt_ms / 2.0);
     printf("  Place order (REST)        : %8.1f ms\n", t_place);
     if (t_cancel > 0)
         printf("  Cancel order (REST)       : %8.1f ms\n", t_cancel);
     printf("  %-44s\n", "------------------------------------------");
     printf("  Total (snapshot+place+cancel) : %8.1f ms\n",
            t_orderbook + t_place + t_cancel);
     printf("\nDone.\n");
 
     curl_easy_cleanup(curl);
     curl_global_cleanup();
     return 0;
 }
 