CC      = gcc
CFLAGS  = -O2 -Wall -Wextra
LIBS    = -lcurl -lwebsockets -lcjson -lssl -lcrypto -lpthread -lm

TARGETS = kalshi_test poly_test arb

.PHONY: all clean

all: $(TARGETS)

kalshi_test: kalshi_test.c
	$(CC) $(CFLAGS) -o $@ $< $(LIBS)

poly_test: poly_test.c
	$(CC) $(CFLAGS) -o $@ $< $(LIBS)

# Arbitrage binary: all arb_*.c files compiled together
# -Wno-deprecated-declarations silences OpenSSL 3.0 EC_KEY_* deprecations
# (same API used by poly_test.c; still functional in OpenSSL 3.x)
ARB_SRCS = arb_main.c arb_config.c arb_ipc.c arb_poly.c arb_kalshi.c
ARB_FLAGS = $(CFLAGS) -Wno-deprecated-declarations

arb: $(ARB_SRCS) arb_config.h arb_ipc.h arb_poly.h arb_kalshi.h
	$(CC) $(ARB_FLAGS) -o $@ $(ARB_SRCS) $(LIBS) -lmariadb

clean:
	rm -f $(TARGETS)
