-- Additive read projection. oi_samples remains the authoritative, unchanged table.
-- Decimal strings with 17 significant digits preserve binary64 values exactly;
-- SQLite's ordinary JSON number formatting can round them to fewer digits.
CREATE TABLE oi_history_days (
    venue TEXT NOT NULL,
    market TEXT NOT NULL,
    symbol TEXT NOT NULL,
    day_ms INTEGER NOT NULL,
    samples TEXT NOT NULL,
    PRIMARY KEY (venue, market, symbol, day_ms)
) WITHOUT ROWID;

CREATE TRIGGER oi_history_days_insert AFTER INSERT ON oi_samples BEGIN
    INSERT INTO oi_history_days VALUES (
        NEW.venue, NEW.market, NEW.symbol, (NEW.bucket_ms / 86400000) * 86400000,
        json_object(CAST(NEW.bucket_ms AS TEXT), json_array(NEW.observed_at_ms,
            printf('%!.17g', NEW.usd_open_interest), NEW.oi_definition,
            printf('%!.17g', NEW.raw_open_interest), NEW.raw_unit, printf('%!.17g', NEW.mark_price))))
    ON CONFLICT (venue, market, symbol, day_ms) DO UPDATE SET samples =
        json_patch(oi_history_days.samples, excluded.samples);
END;

CREATE TRIGGER oi_history_days_update AFTER UPDATE ON oi_samples BEGIN
    INSERT INTO oi_history_days VALUES (
        NEW.venue, NEW.market, NEW.symbol, (NEW.bucket_ms / 86400000) * 86400000,
        json_object(CAST(NEW.bucket_ms AS TEXT), json_array(NEW.observed_at_ms,
            printf('%!.17g', NEW.usd_open_interest), NEW.oi_definition,
            printf('%!.17g', NEW.raw_open_interest), NEW.raw_unit, printf('%!.17g', NEW.mark_price))))
    ON CONFLICT (venue, market, symbol, day_ms) DO UPDATE SET samples =
        json_patch(oi_history_days.samples, excluded.samples);
END;

CREATE TRIGGER oi_history_days_move AFTER UPDATE ON oi_samples
WHEN OLD.venue != NEW.venue OR OLD.market != NEW.market OR OLD.symbol != NEW.symbol OR OLD.bucket_ms != NEW.bucket_ms BEGIN
    UPDATE oi_history_days SET samples = json_remove(samples, '$."' || OLD.bucket_ms || '"')
    WHERE venue = OLD.venue AND market = OLD.market AND symbol = OLD.symbol
        AND day_ms = (OLD.bucket_ms / 86400000) * 86400000;
END;

CREATE TRIGGER oi_history_days_delete AFTER DELETE ON oi_samples BEGIN
    UPDATE oi_history_days SET samples = json_remove(samples, '$."' || OLD.bucket_ms || '"')
    WHERE venue = OLD.venue AND market = OLD.market AND symbol = OLD.symbol
        AND day_ms = (OLD.bucket_ms / 86400000) * 86400000;
END;

-- One statement takes a consistent snapshot while live writers remain serialized
-- by SQLite. Triggers maintain every subsequent insert, correction and deletion.
INSERT INTO oi_history_days
SELECT venue, market, symbol, (bucket_ms / 86400000) * 86400000,
    json_group_object(CAST(bucket_ms AS TEXT), json_array(observed_at_ms,
        printf('%!.17g', usd_open_interest), oi_definition,
        printf('%!.17g', raw_open_interest), raw_unit, printf('%!.17g', mark_price)))
FROM oi_samples GROUP BY venue, market, symbol, (bucket_ms / 86400000) * 86400000
ON CONFLICT (venue, market, symbol, day_ms) DO UPDATE SET samples = excluded.samples;
