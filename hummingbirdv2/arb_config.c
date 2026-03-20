/*
 * Config file parsing and credential loading for hummingbirdv2.
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_config.h"

#include <cjson/cJSON.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ── .env loader ──────────────────────────────────────────────────────────── */

void arb_load_dotenv(const char *path)
{
    FILE *f = fopen(path, "r");
    if (!f) return;

    char line[4096];
    char key[256] = {0};
    char val[4096] = {0};
    int in_entry = 0;

    while (fgets(line, sizeof(line), f)) {
        int len = (int)strlen(line);
        while (len > 0 && (line[len-1] == '\n' || line[len-1] == '\r'))
            line[--len] = '\0';

        if (len == 0) {
            if (in_entry && key[0] && getenv(key) == NULL)
                setenv(key, val, 0);
            in_entry = 0;
            key[0] = val[0] = '\0';
            continue;
        }
        if (line[0] == '#') continue;

        if (!in_entry) {
            const char *p = line;
            if (strncmp(p, "export ", 7) == 0) p += 7;
            const char *eq = strchr(p, '=');
            if (!eq) continue;
            int klen = (int)(eq - p);
            if (klen <= 0 || klen >= (int)sizeof(key)) continue;
            memcpy(key, p, (size_t)klen);
            key[klen] = '\0';
            while (klen > 0 && (key[klen-1] == ' ' || key[klen-1] == '\t')) key[--klen] = '\0';
            strncpy(val, eq + 1, sizeof(val) - 1);
            val[sizeof(val)-1] = '\0';
            in_entry = 1;
        } else {
            strncat(val, "\n", sizeof(val) - strlen(val) - 1);
            strncat(val, line, sizeof(val) - strlen(val) - 1);
        }
    }
    if (in_entry && key[0] && getenv(key) == NULL)
        setenv(key, val, 0);
    fclose(f);
}

void arb_load_creds(ArbCreds *creds)
{
    memset(creds, 0, sizeof(*creds));
    const char *v;

    if ((v = getenv("POLY_ADDRESS")))     strncpy(creds->poly_address,   v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("POLY_API_KEY")))     strncpy(creds->poly_api_key,   v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("POLY_SECRET")))      strncpy(creds->poly_secret,    v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("POLY_PASSPHRASE"))) strncpy(creds->poly_pass,      v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("ETH_PRIV_KEY")))     strncpy(creds->eth_priv_key,   v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("KALSHI_API_KEY_ID"))) strncpy(creds->kalshi_api_key_id, v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("KALSHI_PRIVATE_KEY_PATH"))) strncpy(creds->kalshi_private_key_path, v, ARB_MAX_CRED_LEN - 1);
    if ((v = getenv("KALSHI_PRIVATE_KEY_PEM"))) {
        strncpy(creds->kalshi_private_key_pem, v, ARB_MAX_PEM_LEN - 1);
    } else if (creds->kalshi_private_key_path[0]) {
        FILE *kf = fopen(creds->kalshi_private_key_path, "r");
        if (kf) {
            size_t nread = fread(creds->kalshi_private_key_pem, 1, ARB_MAX_PEM_LEN - 1, kf);
            creds->kalshi_private_key_pem[nread] = '\0';
            fclose(kf);
        }
    }
}

int64_t arb_now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* ── Config JSON ──────────────────────────────────────────────────────────── */

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
    if (fsz <= 0 || fsz > 1024 * 1024) {
        fclose(f);
        return NULL;
    }
    char *buf = malloc((size_t)fsz + 1);
    if (!buf) {
        fclose(f);
        return NULL;
    }
    size_t n = fread(buf, 1, (size_t)fsz, f);
    buf[n] = '\0';
    fclose(f);
    return buf;
}

static int parse_one_pair(cJSON *obj, ArbPairConfig *cfg)
{
    cJSON *tid = cJSON_GetObjectItem(obj, "polymarket_token_id");
    cJSON *no_tid = cJSON_GetObjectItem(obj, "polymarket_no_token_id");
    cJSON *tkr = cJSON_GetObjectItem(obj, "kalshi_ticker");
    cJSON *nr  = cJSON_GetObjectItem(obj, "neg_risk");

    const char *tid_str = cJSON_IsString(tid) ? tid->valuestring : NULL;
    const char *no_tid_str = cJSON_IsString(no_tid) ? no_tid->valuestring : NULL;
    const char *tkr_str = cJSON_IsString(tkr) ? tkr->valuestring : NULL;

    if (!tid_str || !*tid_str || !tkr_str || !*tkr_str) {
        fprintf(stderr, "[config] Each pair must have 'polymarket_token_id' and 'kalshi_ticker'\n");
        return 0;
    }

    memset(cfg, 0, sizeof(*cfg));
    strncpy(cfg->polymarket_token_id, tid_str, ARB_MAX_TOKEN_LEN - 1);
    if (no_tid_str && *no_tid_str)
        strncpy(cfg->polymarket_no_token_id, no_tid_str, ARB_MAX_TOKEN_LEN - 1);
    strncpy(cfg->kalshi_ticker, tkr_str, ARB_MAX_TICKER_LEN - 1);
    if (nr) {
        if (cJSON_IsTrue(nr)) cfg->neg_risk = 1;
        else if (cJSON_IsNumber(nr) && nr->valuedouble != 0.0) cfg->neg_risk = 1;
        else cfg->neg_risk = 0;
    } else {
        cfg->neg_risk = 0;
    }
    return 1;
}

int arb_config_load(const char *path, ArbConfigList *list)
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

    if (cJSON_IsObject(root) && cJSON_GetObjectItem(root, "pairs") != NULL) {
        /* Wrapper: { "pairs": [...], "db_path": "...", ... } */
        cJSON *pairs_arr = cJSON_GetObjectItem(root, "pairs");
        if (cJSON_IsArray(pairs_arr)) {
            int n = cJSON_GetArraySize(pairs_arr);
            for (int i = 0; i < n && list->count < ARB_MAX_PAIRS; i++) {
                cJSON *elem = cJSON_GetArrayItem(pairs_arr, i);
                if (parse_one_pair(elem, &list->pairs[list->count]))
                    list->count++;
            }
        }
        cJSON *db_path = cJSON_GetObjectItem(root, "db_path");
        if (cJSON_IsString(db_path) && db_path->valuestring) {
            strncpy(list->db_path, db_path->valuestring, ARB_MAX_PATH_LEN - 1);
        }
        cJSON *sc = cJSON_GetObjectItem(root, "side_cap");
        if (cJSON_IsNumber(sc) && sc->valuedouble > 0) {
            list->side_cap = sc->valuedouble;
        }
        cJSON *kb = cJSON_GetObjectItem(root, "kalshi_balance");
        if (cJSON_IsNumber(kb) && kb->valuedouble >= 0) {
            list->kalshi_balance = kb->valuedouble;
        }
        cJSON *pb = cJSON_GetObjectItem(root, "poly_balance");
        if (cJSON_IsNumber(pb) && pb->valuedouble >= 0) {
            list->poly_balance = pb->valuedouble;
        }
    } else if (cJSON_IsArray(root)) {
        /* Hummingbird-style: root is array of pair objects */
        int n = cJSON_GetArraySize(root);
        for (int i = 0; i < n && list->count < ARB_MAX_PAIRS; i++) {
            cJSON *elem = cJSON_GetArrayItem(root, i);
            if (parse_one_pair(elem, &list->pairs[list->count]))
                list->count++;
        }
    } else if (cJSON_IsObject(root)) {
        /* Single pair object */
        if (parse_one_pair(root, &list->pairs[0]))
            list->count = 1;
    } else {
        fprintf(stderr, "[config] Root must be a JSON array, object, or { \"pairs\": [...] }\n");
    }

    cJSON_Delete(root);

    if (list->count == 0) {
        fprintf(stderr, "[config] No valid market pairs in '%s'\n", path);
        return 0;
    }

    printf("[config] Loaded %d pair(s) from '%s'\n", list->count, path);
    return list->count;
}
