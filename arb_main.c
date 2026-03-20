/*
 * arb_main.c
 *
 * Entry point for the Polymarket-Kalshi cross-exchange arbitrage program.
 *
 * Usage:
 *   source .env && ./arb [config.json]
 *
 * Config JSON can be:
 *   - A JSON array of pair objects (multi-pair, preferred)
 *   - A single JSON object        (single-pair, backward compatible)
 *
 * Each pair object must contain:
 *   polymarket_token_id  – YES token ID (decimal uint256 string)
 *   kalshi_ticker        – Kalshi market ticker (e.g. KXHIGHNY-26MAR01-T45)
 *   neg_risk             – (optional, bool) true for negRisk Poly markets
 *
 * Credentials are read from environment variables (set via source .env):
 *   POLY_ADDRESS      POLY_API_KEY   POLY_SECRET   POLY_PASSPHRASE
 *   ETH_PRIV_KEY
 *   KALSHI_API_KEY_ID  KALSHI_PRIVATE_KEY_PATH (or KALSHI_PRIVATE_KEY_PEM)
 *
 * Architecture (one pair manager per market pair):
 *
 *   arb_main
 *     └── fork → Pair Manager 0
 *                  ├── fork → kalshi_run (pair 0)
 *                  └── poly_run          (pair 0)
 *     └── fork → Pair Manager 1
 *                  ├── fork → kalshi_run (pair 1)
 *                  └── poly_run          (pair 1)
 *     …
 *
 * Each pair manager creates its own bidirectional pipe pair:
 *   pipe_a  poly→kalshi
 *   pipe_b  kalshi→poly
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_config.h"
#include "arb_ipc.h"
#include "arb_poly.h"
#include "arb_kalshi.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/wait.h>

/* ── Clean-shutdown signal handling ──────────────────────────────────────── */

static volatile sig_atomic_t g_shutdown = 0;
static pid_t g_pair_pids[MAX_PAIRS];
static int   g_pair_count = 0;

static void shutdown_handler(int sig)
{
    (void)sig;
    g_shutdown = 1;
    for (int i = 0; i < g_pair_count; i++)
        if (g_pair_pids[i] > 0)
            kill(g_pair_pids[i], SIGTERM);
}

/* Run one arbitrage pair in the calling process (the "pair manager"). */
static void run_pair(const ArbConfig *cfg, const ArbCreds *creds)
{
    /*
     * Two unidirectional pipes:
     *   pipe_a[0] read  — Kalshi reads (poly→kalshi)
     *   pipe_a[1] write — Poly writes  (poly→kalshi)
     *   pipe_b[0] read  — Poly reads   (kalshi→poly)
     *   pipe_b[1] write — Kalshi writes(kalshi→poly)
     */
    int pipe_a[2], pipe_b[2];
    if (pipe(pipe_a) < 0 || pipe(pipe_b) < 0) {
        perror("pipe");
        _exit(1);
    }

    pid_t kalshi_pid = fork();
    if (kalshi_pid < 0) {
        perror("fork (kalshi)");
        _exit(1);
    }

    if (kalshi_pid == 0) {
        /* ── Kalshi child ── */
        close(pipe_a[1]);  /* don't write poly→kalshi */
        close(pipe_b[0]);  /* don't read  kalshi→poly */
        kalshi_run(cfg, creds, pipe_b[1], pipe_a[0]);
        close(pipe_a[0]);
        close(pipe_b[1]);
        _exit(0);
    }

    /* ── Poly side (pair manager itself) ── */
    close(pipe_a[0]);  /* don't read  poly→kalshi */
    close(pipe_b[1]);  /* don't write kalshi→poly */
    poly_run(cfg, creds, pipe_a[1], pipe_b[0]);
    close(pipe_a[1]);
    close(pipe_b[0]);

    int status;
    waitpid(kalshi_pid, &status, 0);
    printf("[pair %s] Kalshi process exited (status=%d)\n",
           cfg->kalshi_ticker,
           WIFEXITED(status) ? WEXITSTATUS(status) : -1);
}

int main(int argc, char *argv[])
{
    const char *config_path = (argc >= 2) ? argv[1] : "config.json";
    const char *env_path    = ".env";

    load_dotenv(env_path);

    ArbConfigList cfglist;
    if (!load_config_list(config_path, &cfglist)) return 1;

    ArbCreds creds;
    load_env_creds(&creds);

    /* Validate required credentials */
    if (!creds.poly_address[0] || !creds.poly_api_key[0] ||
        !creds.poly_secret[0]  || !creds.poly_pass[0]    ||
        !creds.eth_priv_key[0]) {
        fprintf(stderr,
            "Missing Polymarket credentials.\n"
            "Set POLY_ADDRESS, POLY_API_KEY, POLY_SECRET, POLY_PASSPHRASE, "
            "ETH_PRIV_KEY in .env or environment.\n");
        return 1;
    }
    if (!creds.kalshi_api_key_id[0] || !creds.kalshi_private_key_pem[0]) {
        fprintf(stderr,
            "Missing Kalshi credentials.\n"
            "Set KALSHI_API_KEY_ID and KALSHI_PRIVATE_KEY_PATH (or "
            "KALSHI_PRIVATE_KEY_PEM) in .env or environment.\n");
        return 1;
    }

    printf("=============================================================\n");
    printf(" Polymarket-Kalshi Arbitrage  (%d pair%s)\n",
           cfglist.count, cfglist.count == 1 ? "" : "s");
    printf("=============================================================\n");
    for (int i = 0; i < cfglist.count; i++) {
        printf(" [%d] Poly token : %.40s…\n", i, cfglist.pairs[i].poly_token_id);
        printf("      Kalshi mkt : %s\n",         cfglist.pairs[i].kalshi_ticker);
        printf("      NegRisk    : %s\n",
               cfglist.pairs[i].neg_risk ? "yes" : "no");
    }
    printf("=============================================================\n\n");

    /* Install handlers — SIGTERM/SIGINT both forward to all pair managers */
    g_pair_count = cfglist.count;
    struct sigaction sa = {0};
    sa.sa_handler = shutdown_handler;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT,  &sa, NULL);

    /* Fork one pair manager per market pair */
    for (int i = 0; i < cfglist.count; i++) {
        pid_t pm = fork();
        if (pm < 0) {
            perror("fork (pair manager)");
            for (int j = 0; j < i; j++)
                waitpid(g_pair_pids[j], NULL, 0);
            return 1;
        }
        if (pm == 0) {
            /* Pair manager process — restore default handlers inherited above */
            signal(SIGTERM, SIG_DFL);
            signal(SIGINT,  SIG_DFL);
            run_pair(&cfglist.pairs[i], &creds);
            _exit(0);
        }
        g_pair_pids[i] = pm;
        printf("[main] Started pair manager %d (pid=%d) for %s\n",
               i, (int)pm, cfglist.pairs[i].kalshi_ticker);
    }

    /* Wait for all pair managers */
    for (int i = 0; i < cfglist.count; i++) {
        int status;
        waitpid(g_pair_pids[i], &status, 0);
        printf("[main] Pair manager %d (%s) exited (status=%d)\n",
               i, cfglist.pairs[i].kalshi_ticker,
               WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }

    return 0;
}
