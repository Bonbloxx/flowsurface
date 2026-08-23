CREATE TABLE oi_samples (
    venue TEXT NOT NULL,
    market TEXT NOT NULL,
    symbol TEXT NOT NULL,
    bucket_ms INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    raw_open_interest REAL NOT NULL,
    raw_unit TEXT NOT NULL,
    mark_price REAL NOT NULL,
    usd_open_interest REAL NOT NULL,
    PRIMARY KEY (venue, market, symbol, bucket_ms)
) WITHOUT ROWID;
