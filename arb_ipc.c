/*
 * arb_ipc.c
 *
 * Fixed-size binary message transport over POSIX pipes.
 */
#define _POSIX_C_SOURCE 200809L
#include "arb_ipc.h"

#include <string.h>
#include <errno.h>
#include <sys/select.h>

/* ── ipc_send ─────────────────────────────────────────────────────────────── */

int ipc_send(int fd, const ArbMsg *msg)
{
    const char *p   = (const char *)msg;
    size_t      rem = sizeof(ArbMsg);

    while (rem > 0) {
        ssize_t n = write(fd, p, rem);
        if (n <= 0) return 0;
        p   += n;
        rem -= (size_t)n;
    }
    return 1;
}

/* ── ipc_recv ─────────────────────────────────────────────────────────────── */

int ipc_recv(int fd, ArbMsg *msg)
{
    char  *p   = (char *)msg;
    size_t rem = sizeof(ArbMsg);

    while (rem > 0) {
        ssize_t n = read(fd, p, rem);
        if (n <= 0) return 0;   /* EOF or error */
        p   += n;
        rem -= (size_t)n;
    }
    return 1;
}

/* ── ipc_recv_nb ──────────────────────────────────────────────────────────── */

int ipc_recv_nb(int fd, ArbMsg *msg)
{
    fd_set         rfds;
    struct timeval tv = {0, 0};  /* zero timeout – poll only */

    FD_ZERO(&rfds);
    FD_SET(fd, &rfds);

    int ret = select(fd + 1, &rfds, NULL, NULL, &tv);
    if (ret < 0)  return -1;   /* select error */
    if (ret == 0) return  0;   /* no data */

    return ipc_recv(fd, msg);
}
