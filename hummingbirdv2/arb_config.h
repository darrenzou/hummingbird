/*
 * Config for hummingbirdv2: config.json (like hummingbird) and credentials from .env.
 */
#ifndef ARB_CONFIG_H
#define ARB_CONFIG_H

#include <stdint.h>

#define ARB_MAX_CRED_LEN  512
#define ARB_MAX_PEM_LEN   8192

/*
 * Credentials from environment (set after load_dotenv).
 * Used by Kalshi (REST+WS) and Polymarket (CLOB+WS).
 */
typedef struct {
    char poly_address   [ARB_MAX_CRED_LEN];
    char poly_api_key   [ARB_MAX_CRED_LEN];
    char poly_secret    [ARB_MAX_CRED_LEN];
    char poly_pass      [ARB_MAX_CRED_LEN];
    char eth_priv_key   [ARB_MAX_CRED_LEN];
    char kalshi_api_key_id       [ARB_MAX_CRED_LEN];
    char kalshi_private_key_path [ARB_MAX_CRED_LEN];
    char kalshi_private_key_pem  [ARB_MAX_PEM_LEN];
} ArbCreds;

/* Load .env from path into environment (existing vars take precedence). */
void arb_load_dotenv(const char *path);

/* Fill creds from environment; call after arb_load_dotenv. */
void arb_load_creds(ArbCreds *creds);

/* Unix time in milliseconds (for REST/WS auth and timeouts). */
int64_t arb_now_ms(void);

#define ARB_MAX_PAIRS   64
#define ARB_MAX_TOKEN_LEN 256
#define ARB_MAX_TICKER_LEN 128
#define ARB_MAX_PATH_LEN 256

typedef struct {
    char polymarket_token_id[ARB_MAX_TOKEN_LEN];
    char polymarket_no_token_id[ARB_MAX_TOKEN_LEN];  /* optional: NO token for sell-first hedge */
    char kalshi_ticker[ARB_MAX_TICKER_LEN];
    int  neg_risk;  /* 1 if Poly market uses negRisk CTF */
} ArbPairConfig;

typedef struct {
    ArbPairConfig pairs[ARB_MAX_PAIRS];
    int           count;

    /* Optional globals (0 or empty = use default). */
    char   db_path[ARB_MAX_PATH_LEN];
    double side_cap;         /* 0 = default 1000 */
    double kalshi_balance;   /* 0 = use live balance / default */
    double poly_balance;     /* 0 = use config/env default */
} ArbConfigList;

/*
 * Parse config JSON at path. Accepts:
 *   - Object with "pairs" array and optional "db_path", "side_cap", "kalshi_balance", "poly_balance"
 *   - Array of pair objects (hummingbird-style; globals stay default)
 *   - Single pair object (one pair; globals stay default)
 * Returns number of pairs loaded (>= 1), or 0 on failure.
 */
int arb_config_load(const char *path, ArbConfigList *list);

#endif /* ARB_CONFIG_H */
