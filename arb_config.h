/*
 * arb_config.h
 *
 * Shared types, credentials, and time helpers for the Polymarket-Kalshi
 * cross-exchange arbitrage program.
 */
#pragma once

#define _POSIX_C_SOURCE 200809L
#include <stdint.h>
#include <time.h>

/* ── Time helpers ─────────────────────────────────────────────────────────── */

typedef struct timespec hr_time_t;

static inline void hr_now(hr_time_t *t)
{
    clock_gettime(CLOCK_MONOTONIC, t);
}

static inline double hr_ms(const hr_time_t *a, const hr_time_t *b)
{
    return (b->tv_sec  - a->tv_sec ) * 1000.0
         + (b->tv_nsec - a->tv_nsec) / 1e6;
}

static inline int64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* ── Config ───────────────────────────────────────────────────────────────── */

#define MAX_TOKEN_ID_LEN 256
#define MAX_TICKER_LEN   128
#define MAX_CRED_LEN     512
#define MAX_PEM_LEN      8192

/*
 * Per-market configuration loaded from config.json.
 *   polymarket_token_id  – YES token ID (uint256 as decimal string)
 *   kalshi_ticker        – Kalshi market ticker, e.g. KXHIGHNY-26MAR01-T45
 *   neg_risk             – 1 if the Poly market uses the negRisk CTF contract
 */
typedef struct {
    char poly_token_id[MAX_TOKEN_ID_LEN];
    char kalshi_ticker[MAX_TICKER_LEN];
    int  neg_risk;
} ArbConfig;

/*
 * Credentials loaded from environment variables (populated from .env).
 * All fields are null-terminated strings.
 */
typedef struct {
    /* Polymarket */
    char poly_address  [MAX_CRED_LEN];  /* 0x… proxy wallet address        */
    char poly_api_key  [MAX_CRED_LEN];  /* L2 API key UUID                 */
    char poly_secret   [MAX_CRED_LEN];  /* L2 API secret (base64)          */
    char poly_pass     [MAX_CRED_LEN];  /* L2 API passphrase               */
    char eth_priv_key  [MAX_CRED_LEN];  /* hex private key (with/without 0x)*/

    /* Kalshi */
    char kalshi_api_key_id       [MAX_CRED_LEN]; /* UUID                   */
    char kalshi_private_key_path [MAX_CRED_LEN]; /* path to .pem file      */
    char kalshi_private_key_pem  [MAX_PEM_LEN];  /* PEM content (in-memory)*/
} ArbCreds;

/* ── Multi-pair config list ───────────────────────────────────────────────── */

#define MAX_PAIRS 64

/*
 * Holds all market pairs loaded from the config file.
 * The config JSON may be either:
 *   - A JSON array of pair objects  (preferred, multi-pair)
 *   - A single JSON object          (backward-compatible, treated as 1 pair)
 */
typedef struct {
    ArbConfig pairs[MAX_PAIRS];
    int       count;
} ArbConfigList;

/* ── Function declarations ────────────────────────────────────────────────── */

/* Parse .env file into process environment (existing vars take precedence). */
void load_dotenv(const char *path);

/* Populate creds from environment variables.  Call after load_dotenv(). */
void load_env_creds(ArbCreds *creds);

/*
 * Parse config JSON file at `path` into a list of market pairs.
 * Accepts either a JSON array or a single JSON object.
 * Returns the number of pairs loaded (≥ 1), or 0 on failure.
 */
int load_config_list(const char *path, ArbConfigList *list);

/*
 * Parse config JSON file at `path` into a single `cfg`.
 * Convenience wrapper around load_config_list for single-pair callers.
 * Returns 1 on success, 0 on failure.
 */
int load_config(const char *path, ArbConfig *cfg);
