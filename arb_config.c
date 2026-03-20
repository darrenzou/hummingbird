/*
 * arb_config.c
 *
 * Config file parsing and credential loading for the arbitrage program.
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_config.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <cjson/cJSON.h>

/* ── .env loader ──────────────────────────────────────────────────────────── */

void load_dotenv(const char *path)
{
    FILE *f = fopen(path, "r");
    if (!f) return;

    char line[4096];
    char key[256]  = {0};
    char val[4096] = {0};
    int  in_entry  = 0;

    while (fgets(line, sizeof(line), f)) {
        /* strip trailing CR/LF */
        int len = (int)strlen(line);
        while (len > 0 && (line[len-1] == '\n' || line[len-1] == '\r'))
            line[--len] = '\0';

        /* blank line ends the current entry → flush it */
        if (len == 0) {
            if (in_entry && key[0] && getenv(key) == NULL)
                setenv(key, val, 0);
            in_entry = 0;
            key[0] = val[0] = '\0';
            continue;
        }

        /* skip comment lines */
        if (line[0] == '#') continue;

        if (!in_entry) {
            /* strip optional "export " prefix */
            const char *p = line;
            if (strncmp(p, "export ", 7) == 0) p += 7;

            const char *eq = strchr(p, '=');
            if (!eq) continue;

            int klen = (int)(eq - p);
            if (klen <= 0 || klen >= (int)sizeof(key)) continue;
            memcpy(key, p, (size_t)klen);
            key[klen] = '\0';
            /* trim trailing whitespace from key */
            while (klen > 0 && (key[klen-1] == ' ' || key[klen-1] == '\t'))
                key[--klen] = '\0';

            strncpy(val, eq + 1, sizeof(val) - 1);
            in_entry = 1;
        } else {
            /* continuation line of a multi-line value (e.g. RSA PEM block) */
            strncat(val, "\n",  sizeof(val) - strlen(val) - 1);
            strncat(val, line,  sizeof(val) - strlen(val) - 1);
        }
    }

    /* flush last entry if file does not end with blank line */
    if (in_entry && key[0] && getenv(key) == NULL)
        setenv(key, val, 0);

    fclose(f);
}

/* ── Credential loader ────────────────────────────────────────────────────── */

void load_env_creds(ArbCreds *creds)
{
    memset(creds, 0, sizeof(*creds));

    const char *v;

    /* Polymarket */
    if ((v = getenv("POLY_ADDRESS")))    strncpy(creds->poly_address,  v, sizeof(creds->poly_address)  - 1);
    if ((v = getenv("POLY_API_KEY")))    strncpy(creds->poly_api_key,  v, sizeof(creds->poly_api_key)  - 1);
    if ((v = getenv("POLY_SECRET")))     strncpy(creds->poly_secret,   v, sizeof(creds->poly_secret)   - 1);
    if ((v = getenv("POLY_PASSPHRASE"))) strncpy(creds->poly_pass,     v, sizeof(creds->poly_pass)     - 1);
    if ((v = getenv("ETH_PRIV_KEY")))    strncpy(creds->eth_priv_key,  v, sizeof(creds->eth_priv_key)  - 1);

    /* Kalshi */
    if ((v = getenv("KALSHI_API_KEY_ID")))
        strncpy(creds->kalshi_api_key_id,
                v, sizeof(creds->kalshi_api_key_id) - 1);
    if ((v = getenv("KALSHI_PRIVATE_KEY_PATH")))
        strncpy(creds->kalshi_private_key_path,
                v, sizeof(creds->kalshi_private_key_path) - 1);

    /* PEM: prefer inline env var, else read from key file */
    if ((v = getenv("KALSHI_PRIVATE_KEY_PEM"))) {
        strncpy(creds->kalshi_private_key_pem,
                v, sizeof(creds->kalshi_private_key_pem) - 1);
    } else if (creds->kalshi_private_key_path[0]) {
        FILE *kf = fopen(creds->kalshi_private_key_path, "r");
        if (kf) {
            fread(creds->kalshi_private_key_pem, 1,
                  sizeof(creds->kalshi_private_key_pem) - 1, kf);
            fclose(kf);
        }
    }
}

/* ── Config file parser ───────────────────────────────────────────────────── */

/* Parse a single JSON object into one ArbConfig.  Returns 1 on success. */
static int parse_one_pair(cJSON *obj, ArbConfig *cfg)
{
    const char *tid = cJSON_GetStringValue(cJSON_GetObjectItem(obj, "polymarket_token_id"));
    const char *tkr = cJSON_GetStringValue(cJSON_GetObjectItem(obj, "kalshi_ticker"));
    cJSON      *nr  = cJSON_GetObjectItem(obj, "neg_risk");

    if (!tid || !*tid || !tkr || !*tkr) {
        fprintf(stderr,
            "[config] Each pair must have 'polymarket_token_id' and 'kalshi_ticker'\n");
        return 0;
    }

    memset(cfg, 0, sizeof(*cfg));
    strncpy(cfg->poly_token_id, tid, sizeof(cfg->poly_token_id) - 1);
    strncpy(cfg->kalshi_ticker, tkr, sizeof(cfg->kalshi_ticker) - 1);
    cfg->neg_risk = (nr && cJSON_IsTrue(nr)) ? 1 : 0;
    return 1;
}

/* Read the entire file into a heap-allocated buffer; caller frees. */
static char *read_file(const char *path)
{
    FILE *f = fopen(path, "r");
    if (!f) {
        fprintf(stderr, "[config] Cannot open '%s'\n", path);
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long fsz = ftell(f);
    fseek(f, 0, SEEK_SET);

    char *buf = malloc((size_t)fsz + 1);
    if (!buf) { fclose(f); return NULL; }
    fread(buf, 1, (size_t)fsz, f);
    buf[fsz] = '\0';
    fclose(f);
    return buf;
}

int load_config_list(const char *path, ArbConfigList *list)
{
    memset(list, 0, sizeof(*list));

    char *buf = read_file(path);
    if (!buf) return 0;

    cJSON *root = cJSON_Parse(buf);
    free(buf);

    if (!root) {
        fprintf(stderr, "[config] Failed to parse JSON in '%s'\n", path);
        return 0;
    }

    if (cJSON_IsArray(root)) {
        /* Array of pair objects */
        int n = cJSON_GetArraySize(root);
        for (int i = 0; i < n && list->count < MAX_PAIRS; i++) {
            cJSON *elem = cJSON_GetArrayItem(root, i);
            if (parse_one_pair(elem, &list->pairs[list->count]))
                list->count++;
        }
    } else if (cJSON_IsObject(root)) {
        /* Single object — backward compatible */
        if (parse_one_pair(root, &list->pairs[0]))
            list->count = 1;
    } else {
        fprintf(stderr, "[config] Root must be a JSON array or object\n");
    }

    cJSON_Delete(root);

    if (list->count == 0) {
        fprintf(stderr, "[config] No valid market pairs found in '%s'\n", path);
        return 0;
    }

    printf("[config] Loaded %d market pair(s) from '%s'\n", list->count, path);
    return list->count;
}

int load_config(const char *path, ArbConfig *cfg)
{
    ArbConfigList list;
    if (!load_config_list(path, &list)) return 0;
    *cfg = list.pairs[0];
    return 1;
}
