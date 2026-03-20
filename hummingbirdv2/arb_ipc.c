#define _POSIX_C_SOURCE 200809L
#include "arb_ipc.h"

#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <stdio.h>
#include <sys/select.h>

static int write_full(int fd, const void *buf, size_t len)
{
    const char *p = (const char *)buf;
    size_t remaining = len;
    while (remaining > 0) {
        ssize_t n = write(fd, p, remaining);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            perror("write");
            return -1;
        }
        if (n == 0) {
            /* Should not happen for blocking fd; treat as error/EOF. */
            fprintf(stderr, "write returned 0 bytes\n");
            return -1;
        }
        p += n;
        remaining -= (size_t)n;
    }
    return 0;
}

static int read_full(int fd, void *buf, size_t len)
{
    char *p = (char *)buf;
    size_t remaining = len;
    while (remaining > 0) {
        ssize_t n = read(fd, p, remaining);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            perror("read");
            return -1;
        }
        if (n == 0) {
            /* EOF */
            return -1;
        }
        p += n;
        remaining -= (size_t)n;
    }
    return 0;
}

int arb_ipc_send(int fd, const ArbMsg *msg)
{
    if (!msg) return -1;
    return write_full(fd, msg, sizeof(*msg));
}

int arb_ipc_recv(int fd, ArbMsg *msg)
{
    if (!msg) return -1;
    return read_full(fd, msg, sizeof(*msg));
}

int arb_ipc_poll(int fd, int timeout_ms)
{
    if (fd < 0 || (unsigned)fd >= FD_SETSIZE)
        return -1;
    fd_set rfds;
    struct timeval tv;
    FD_ZERO(&rfds);
    FD_SET(fd, &rfds);
    tv.tv_sec = timeout_ms / 1000;
    tv.tv_usec = (timeout_ms % 1000) * 1000;
    int r = select(fd + 1, &rfds, NULL, NULL, &tv);
    if (r < 0) return -1;
    return (r > 0 && FD_ISSET(fd, &rfds)) ? 1 : 0;
}


