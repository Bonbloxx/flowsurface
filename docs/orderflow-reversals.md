# Absorption & Exhaustion

Enable **Absorption & Exhaustion** in a Binance **BTCUSDT perpetual** candle or
time-based footprint pane. Pane settings contain its controls. Analysis uses
executions, not inferred candle volume. Other markets and tick/Renko/TPO charts
do not offer this indicator yet.

Confirmed absorption is enabled by default. Exhaustion is an optional
experimental detector: its forward results were weaker, so it is disabled by
default. This is a context indicator, not an automated entry strategy.

## Evidence and confirmation

- Absorption requires an established directional approach to a three-minute
  extreme, unusual aggressor volume concentrated in one narrow price band,
  at least 70% aggressor dominance, repeated trading there, and concentration
  relative to the surrounding flow. The threshold is the larger of the dollar
  floor and a past-only five-minute adaptive volume baseline.
- Exhaustion requires an established approach, declining aggressor volume
  through three bands toward the extreme, and a substantial fall in execution
  pace. Low total volume by itself is insufficient.
- Both require a later rejection of at least three analysis bands by default
  (or 15% of the three-minute range, whichever is larger), plus opposing
  aggressive flow. Unconfirmed observations expire after 45 seconds or an
  invalidating extension. Same-side cooldowns suppress clusters of repeats.

Squares mean absorption; diamonds mean exhaustion. Blue/positive marks expect
an upward reaction and red/negative marks expect a downward reaction, using the
existing theme's orderflow colors. Filled marks have confirmed. A level breach
within five minutes fades the mark rather than deleting it. A faded mark can
still have produced an earlier reaction; this status is not a trade PnL result.
Optional hollow observations have not confirmed yet.

Marks sit on the **confirmation candle**, at the observed price extreme. Hover
shows actual observation and confirmation times, side volumes, threshold,
pace ratio and rejection distance. Confirmation uses the close of an execution
second; live publication waits approximately two additional seconds to tolerate
small batch reordering. Confirmed seconds are immutable. A configuration change
recalculates the retained history; panning, chart timeframe and footprint display
row changes do not change the detector's analysis grid.

Executions support evidence of absorption; they cannot identify a hidden order
or prove an iceberg without order-level data. Dollar amounts are BTCUSDT quote
notional, normalized once from the adapter's base/quote quantity convention.

## History and resource limits

Enabling the indicator requests **four hours plus five minutes of warm-up**.
It uses the existing connector and recorder coverage protocol. Binance's
existing rate-limited archive/REST path fills missing recorder coverage, or
supplies the entire range when general trade backfill is Off. This exception
is scoped to this bounded Binance indicator; other indicators retain their
existing history policy. Network failures/partial history stay incomplete and
retry with the existing cooldown. Initial direct Binance fetching can take
longer than recorder fetching on a busy market.

The indicator retains one-second summaries and price-band totals, not raw
executions or a second UTC-day footprint book. Retention is four hours plus ten
minutes; each second has at most 32 bands, and confirmed events are capped at
512. A band-overflow second, explicit disconnect, or silence exceeding ten
seconds resets continuity and warm-up. Short genuine idle intervals do not
count as exhaustion. Historical pages are staged and committed atomically;
their overlap replaces the live book once. Fetch handles belong to this
indicator and are cancelled on disable/grid/source changes.

Runtime measurements and their limitations are in the
[verification report](evidence/orderflow-btc-verification-2026-10-08.json).

## Replay results

The [replay receipt](evidence/orderflow-btc-replay-2026-10-08.json) contains input
URLs, verified Binance SHA-256 checksums, configuration, every evaluated mark
and failures. Four full archive days contain 4,051,539 aggregate executions.
With the final default detector, 20 of 32 observations reached a 1R target
before a 1R stop within five minutes; a single time-shifted control reached
16 of 32. The entry calculation includes the two-second publication buffer.
The frozen-detector fourth day produced four marks, three of which passed that
criterion, including substantial reactions in both directions.

These are small, selected event-study samples. They do not establish a stable
edge or profitability. Calibration used October 5; October 6 and September 30
informed the decision to disable exhaustion by default. October 4 was evaluated
after the detector was frozen. Fees, slippage, fill quality, and actual trade
execution are not modeled. The control is descriptive, not statistical
qualification. Timeout and same-second ambiguous barriers count conservatively.

Reproduce using an extracted Binance USD-M aggTrades CSV:

```powershell
cargo run -p flowsurface-data --release --example orderflow_replay -- BTCUSDT-aggTrades-2026-10-04.csv
```

An optional second argument is a JSON detector configuration, e.g.
`{"exhaustion":true}` to inspect the experimental detector. CSV/archive files
are not checked into Git. The replay harness calls the production detector.

## Adding Bybit and Hyperliquid later

The analysis domain is venue-neutral. The first runtime explicitly accepts only
Binance BTCUSDT and routes source identities through existing `TickerInfo` and
trade streams. Extend it through `data::aggregation` and the existing recorder
history, using a common BTC quote-notional grid and UTC seconds. Keep each
venue's readiness/gaps/deduplication separate before combining volume, and
define an explicit price reference. Do not simply sum differently priced venue
extrema or reinterpret a venue failure as exhaustion. No additional transport,
database, GUI framework, or crate is needed for the current implementation.
