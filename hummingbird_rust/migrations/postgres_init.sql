-- Optional reference schema (also applied at runtime via ArbDb::run_schema_pg).
CREATE TABLE IF NOT EXISTS events (
    id BIGSERIAL PRIMARY KEY,
    ts_ms BIGINT NOT NULL,
    venue VARCHAR(16) NOT NULL,
    event_type VARCHAR(32) NOT NULL,
    market TEXT,
    severity VARCHAR(32),
    http_status INT,
    message TEXT,
    context_json TEXT,
    fill_price_cents INT,
    fill_amount INT,
    abort_reason TEXT
);

CREATE TABLE IF NOT EXISTS orderbook_snapshots (
    id BIGSERIAL PRIMARY KEY,
    ts_ms BIGINT NOT NULL,
    market TEXT,
    poly_book_json TEXT NOT NULL,
    kalshi_book_json TEXT NOT NULL,
    trigger_event_id BIGINT REFERENCES events(id)
);
