-- Existing Bybit samples were collected from single-side fields. Keeping the
-- stored value plus an explicit semantic marker makes the read correction
-- race-safe while new samples use Bybit's venue-published both-side fields.
ALTER TABLE oi_samples
ADD COLUMN oi_definition TEXT NOT NULL DEFAULT 'legacy';
