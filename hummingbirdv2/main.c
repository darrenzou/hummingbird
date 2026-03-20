#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <unistd.h>
#include <sys/wait.h>
#include <stdlib.h>
#include <string.h>

#include "arb_config.h"
#include "arb_poly.h"
#include "arb_kalshi.h"

int main(int argc, char *argv[])
{
    const char *config_path = (argc >= 2) ? argv[1] : "config.json";

    arb_load_dotenv(".env");
    ArbCreds creds;
    arb_load_creds(&creds);

    ArbConfigList config;
    if (arb_config_load(config_path, &config) == 0) {
        return 1;
    }

    /* Require Kalshi and Poly creds per plan (no simulation). */
    if (!creds.kalshi_api_key_id[0] || !creds.kalshi_private_key_pem[0]) {
        fprintf(stderr, "[main] Kalshi creds required (KALSHI_API_KEY_ID, KALSHI_PRIVATE_KEY_PATH or PEM in .env)\n");
        return 1;
    }
    if (!creds.poly_address[0] || !creds.eth_priv_key[0]) {
        fprintf(stderr, "[main] Poly creds required (POLY_ADDRESS, ETH_PRIV_KEY, etc. in .env)\n");
        return 1;
    }
    setenv("ARB_LIVE", "1", 1);
    setenv("ARB_TICKER", config.pairs[0].kalshi_ticker, 1);
    setenv("ARB_TOKEN_ID", config.pairs[0].polymarket_token_id, 1);
    if (config.pairs[0].polymarket_no_token_id[0]) {
        setenv("ARB_NO_TOKEN_ID", config.pairs[0].polymarket_no_token_id, 1);
    }
    setenv("ARB_NEG_RISK", config.pairs[0].neg_risk ? "1" : "0", 1);
    printf("[main] ticker=%s token=%s\n", config.pairs[0].kalshi_ticker, config.pairs[0].polymarket_token_id);

    /* Apply globals to environment so child processes see them. */
    if (config.db_path[0]) {
        setenv("ARB_DB_PATH", config.db_path, 1);
    }
    if (config.side_cap > 0) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%.0f", config.side_cap);
        setenv("ARB_SIDE_CAP", buf, 1);
    }
    if (config.kalshi_balance > 0) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%.0f", config.kalshi_balance);
        setenv("ARB_KALSHI_BALANCE", buf, 1);
    }
    if (config.poly_balance > 0) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%.0f", config.poly_balance);
        setenv("ARB_POLY_BALANCE", buf, 1);
    }

    int poly_to_kalshi[2];
    int kalshi_to_poly[2];

    if (pipe(poly_to_kalshi) < 0) {
        perror("pipe poly_to_kalshi");
        return 1;
    }
    if (pipe(kalshi_to_poly) < 0) {
        perror("pipe kalshi_to_poly");
        return 1;
    }

    pid_t poly_pid = fork();
    if (poly_pid < 0) {
        perror("fork poly");
        return 1;
    }
    if (poly_pid == 0) {
        /* Poly child process. */
        close(poly_to_kalshi[0]);   /* close read end */
        close(kalshi_to_poly[1]);   /* close write end */

        int fd_out = poly_to_kalshi[1];
        int fd_in  = kalshi_to_poly[0];

        poly_process_run(fd_in, fd_out);

        close(fd_out);
        close(fd_in);
        _exit(0);
    }

    pid_t kalshi_pid = fork();
    if (kalshi_pid < 0) {
        perror("fork kalshi");
        return 1;
    }
    if (kalshi_pid == 0) {
        /* Kalshi child process. */
        close(poly_to_kalshi[1]);   /* close write end */
        close(kalshi_to_poly[0]);   /* close read end */

        int fd_in  = poly_to_kalshi[0];
        int fd_out = kalshi_to_poly[1];

        kalshi_process_run(fd_in, fd_out);

        close(fd_out);
        close(fd_in);
        _exit(0);
    }

    /* Parent: close all pipe ends and wait for children. */
    close(poly_to_kalshi[0]);
    close(poly_to_kalshi[1]);
    close(kalshi_to_poly[0]);
    close(kalshi_to_poly[1]);

    int status;
    waitpid(poly_pid, &status, 0);
    waitpid(kalshi_pid, &status, 0);

    printf("[parent] both child processes exited\n");
    return 0;
}
