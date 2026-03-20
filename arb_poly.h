/*
 * arb_poly.h
 *
 * Public interface for the Polymarket process of the arbitrage program.
 */
#pragma once

#include "arb_config.h"
#include "arb_ipc.h"

/*
 * poly_run – main entry point for the Polymarket process.
 *
 * Connects to the Polymarket WebSocket, gets the initial book snapshot,
 * coordinates with the Kalshi process via IPC pipes, pre-signs orders,
 * and places them as Kalshi limit orders are filled.
 *
 * fd_to_kalshi   – write-end of the Poly→Kalshi pipe
 * fd_from_kalshi – read-end  of the Kalshi→Poly pipe
 *
 * Does not return until an abort condition is reached or a fatal error.
 */
void poly_run(const ArbConfig *cfg, const ArbCreds *creds,
              int fd_to_kalshi, int fd_from_kalshi);
