CC      := gcc
CFLAGS  := -O2 -std=c11 -Wall -Wextra -Wpedantic -Werror -Wno-unused-parameter
LDFLAGS := -lm -lsqlite3 -lcjson -lcurl -lwebsockets -lssl -lcrypto -lpthread

SRC := \
    arb_ipc.c \
    arb_config.c \
    arb_db.c \
    kalshi_live.c \
    poly_live.c \
    arb_poly.c \
    arb_kalshi.c \
    main.c

OBJ := $(SRC:.c=.o)

TARGET := arb_bot

.PHONY: all clean

all: $(TARGET)

$(TARGET): $(OBJ)
	$(CC) $(CFLAGS) -o $@ $^ $(LDFLAGS)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(OBJ) $(TARGET)

