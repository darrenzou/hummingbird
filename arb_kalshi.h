/*
 * arb_kalshi.h
 *
 * Public interface for the Kalshi process of the arbitrage program.
 */
#pragma once

#include "arb_config.h"
#include "arb_ipc.h"

/*
 * kalshi_run – main entry point for the Kalshi process (child after fork).
 *
 * Connects to the Kalshi WebSocket, subscribes to orderbook_delta, waits
 * for the Poly book snapshot via IPC, performs the arb check, coordinates
 * the pre-signing confirmation loop, places limit orders, monitors fills
 * via the user_fills channel, and relays fill notifications to Poly.
 *
 * fd_to_poly   – write-end of the Kalshi→Poly pipe
 * fd_from_poly – read-end  of the Poly→Kalshi pipe
 *
 * Does not return until an abort condition is reached or a fatal error.
 */
void kalshi_run(const ArbConfig *cfg, const ArbCreds *creds,
                int fd_to_poly, int fd_from_poly);
