# Absorption & Exhaustion

Enable **Absorption & Exhaustion** in a Binance **BTCUSDT perpetual** candle or
time-based footprint pane. Analysis uses executions, not inferred candle volume.
Other markets and tick/Renko/TPO charts do not offer this indicator yet.

The current policy is **adaptive-v1**. There are no user-adjustable detection
presets, dollar floors, sensitivity sliders or price-band widths. Settings only
show/hide absorption, exhaustion and unconfirmed observations. Both families
are always calculated with the same automatic rules. Old saved tuning values
still deserialize for layout compatibility but do not affect the detector.
Automatic does not mean parameter-free: internal ratios, time windows and
continuity rules define a versioned policy. This is a context indicator, not an
automated entry strategy. Exhaustion remains experimental.

## Adaptive evidence and confirmation

- The grid follows the **preceding three-minute price range**, divided into 24
  bands. Exchange tick precision and recent compressed-summary resolution set
  the minimum width. There is no fixed BTC price or dollar-width floor.
  Analysis is independent of candle timeframe, zoom and footprint display rows.
- Activity baselines are past-only five-minute exponentially weighted means
  and variances, separate for buying and selling. Absorption uses maximum
  aggressor band volume; exhaustion uses three-second aggressor flow. There is
  no minimum fixed dollar notional.
- Absorption requires a directional approach to a three-minute extreme,
  unusual aggressor volume (band baseline mean + two standard deviations),
  at least 70% aggressor dominance, trading in at least three seconds there,
  and concentration relative to surrounding flow.
- Exhaustion requires a meaningful prior push (three-second baseline mean +
  0.75 standard deviations), tapering through three bands toward the extreme,
  and a **65% or greater fall** in three-second aggressor pace. A quiet market
  or one low-volume print alone is insufficient.
- Both require a later rejection of four observation-time bands or 15% of the
  three-minute range, whichever is larger, plus opposing aggressive flow.
  Exhaustion also needs **at least two opposing-flow seconds**, meaningful
  opposing activity relative to its normal pace, and original pressure still
  below half the prior push. Resumed original aggression blocks confirmation
  even if price briefly bounces.
- Unconfirmed observations expire after 45 seconds or an invalidating
  extension. Same-side cooldowns suppress clusters. Upward and downward pushes
  use symmetric rules. No session-specific tuning or daily quota is used.

Band width, activity evidence and rejection requirements freeze at observation.
Later volatility cannot resize old evidence. Prices and distance comparisons
use integer exchange price units. Mirrored-trend tests and full-day
price/activity unit rescaling produce identical sides and event timestamps;
minimum exchange tick precision remains a physical resolution limit.

Squares mean absorption; diamonds mean exhaustion. Blue/positive marks expect
an upward reaction and red/negative marks expect a downward reaction, using the
theme's orderflow colors. Filled marks have confirmed. Every confirmed mark
keeps its original side color, fill and size, including signals whose level
later breaks. Later outcomes never recolor, fade or delete a confirmed mark.
Optional hollow observations have not confirmed yet. Five-minute level breaches
remain recorded internally for analysis; they do not change chart presentation
and are not trade PnL.

Markers span 18-28 screen pixels and retain that size when zooming. Absorption
squares grow with aggressor notional relative to their adaptive threshold: 1x
is smallest and 4x reaches the cap. Exhaustion diamonds grow with the pace
collapse: a 65% drop is smallest and a 100% drop reaches the cap. Size describes
intensity, not probability or profit, and stays fixed after observation.

Marks sit on the **confirmation candle**, at the observed extreme. Hover shows
observation/confirmation times, side volumes, adaptive threshold, pace and band
width. Confirmation uses an execution-second close; publication waits about
two additional seconds for small batch reordering. Visibility switches do not
replay or refetch history. Panning and chart timeframe changes do not change
evidence. Source changes rebuild through the existing indicator path.

Executions support evidence of absorption; they cannot identify a hidden order
or prove an iceberg without order-level data. Dollar amounts are BTCUSDT quote
notional, normalized once from the adapter's base/quote quantity convention.

## Before/after comparison

The [adaptive receipt](evidence/orderflow-adaptive-2026-10-08.json) contains
baseline identity, verified archive checksums, 24 session comparisons, both
families, every assessed full-day mark, rejected variants and resources.
Six full UTC archives contain 6,357,227 aggregate executions. The baseline is
the previous 24-hour **Balanced** detector at commit `94f735f`.

| NY session, 09:30-16:00 EDT | Previous marks | Adaptive marks | Previous 1R reactions | Adaptive 1R reactions |
| --- | ---: | ---: | ---: | ---: |
| September 28, separate validation | 7 | 9 | 2 / 7 | 8 / 9 |
| September 29, separate validation | 10 | 8 | 5 / 10 | 4 / 8 |
| October 1, development | 15 | 15 | 9 / 15 | 9 / 15 |
| October 3, quiet weekend/development | 2 | 3 | 2 / 2 | 1 / 3 |
| October 6, development | 9 | 10 | 5 / 9 | 6 / 10 |
| October 7, development | 10 | 6 | 7 / 10 | 4 / 6 |

Reactions mean a 1R target before a 1R stop within five minutes, measured from
an entry **after confirmation plus the two-second publication delay**.
Same-second ambiguity and timeouts fail. Risk is distance to the observed
extreme plus one observation-time band; risk widths differ between policies.

All six NY sessions: **32/51 adaptive versus 30/53 previous** gross reactions.
The two separate validation NY sessions: **12/17 versus 7/17**, with shifted
controls of 7/17 and 4/17. At a hypothetical extra five-basis-point target hurdle,
validation counts were 8/17 versus 4/17. Exhaustion alone produced 5/6 versus
7/15 gross reactions on these two NY sessions. These are small selected samples,
not stable hit-rate estimates.

Active-session usefulness is the primary comparison. Quieter periods are
reported separately: weaker performance there is an expected operating
limitation to assess, not automatically a detector defect. Session labels are
for evaluation only; the detector follows observed flow without a clock-time
gate. Execution count is an activity proxy, not a measurement of L2 liquidity.

**The wider result is mixed.** Full-day gross reactions were **91/166 adaptive
versus 103/181 previous**. Validation full days were 26/58 versus 38/76; the
adaptive shifted control was 27/58. Quiet-weekend frequency increased without
better quality. This does not demonstrate universal improvement, a trading edge
or profitability. No quota forces prints into quiet sessions. Fees, spread,
slippage and fills are not modeled; 2/5 bps hurdles are hypothetical sensitivity
scenarios, not net trading PnL.

Development examined 13 variants. Extra pre-observation delay made V-turns too
rare; sustained weakening is instead checked at confirmation. Full-width outer
bands corrected a directional boundary bias. Volume-conserving compaction
avoids treating fast price movement as missing data. Policy coefficients froze
before September 28/29 were inspected. Subsequent integer-price precision repairs
changed boundary events; this table uses the corrected production result. No
outcome-based coefficient tuning followed validation. This is not a fully
untouched final-code holdout.

## History and resources

The indicator requests **four days plus five minutes of warm-up**, matching the
four-day Daily Delta span in the current layout, through the
existing connector and recorder coverage protocol. Binance's existing bounded
archive/REST path fills missing coverage or supplies the range when general
trade backfill is Off. This exception remains scoped to this Binance indicator.
Partial history stays incomplete and retries through the existing cooldown.

History is replayed in chronological minute slices inside the existing
10,000-trade messages. Only ten minutes of staging summaries are kept, while
the rolling detector retains four days of signal records. Partial loads stay
private until complete coverage succeeds. Replay and live use the same
two-second publication buffer; previously processed seconds are immutable.

Only one-second summaries and band totals are retained, not raw executions or
a second footprint day book. The live summary book retains its existing bound
of 24 hours plus ten minutes, with **16 bands per second**. Signals retain four
days plus ten minutes, capped at **2,048 events**. Overflowing seconds widen
their storage grid and merge adjacent bands in place, preserving all buying
and selling notional. Analysis respects the compressed resolution. Numerical
overflow, disconnects or silence over ten seconds reset continuity and warm-up.
Genuine idle intervals do not become exhaustion. History stages atomically.
Only signals from the last five minutes are inspected for level breaches, so
older retained marks do not increase the per-second outcome-check cost.

Recorder loads keep one shared history slot, sequential 50,000-row pages and
a 200 ms pause per page. Ingestion yields in 10,000-trade UI messages. Capped
pages re-fetch their final millisecond to preserve ties. No raw-trade disk
cache or server-side export file is added.

The original 24-hour adaptive replay measured October 7 compact memory at
**26.3 MiB versus 21.1 MiB previously**. The busier
validation day uses 28.3 MiB. Release detector rebuilds take about 175-195 ms,
versus roughly 60 ms previously; live summary processing p99 is 2.9-3.8
microseconds. These are detector costs, not whole-app frame times. The 16-band
cap lowers the estimated worst-case retained book from 77.7 to **46.5 MiB**.
The four-day streaming load does not multiply that book by four: staging holds
ten minutes plus at most one minute slice, and retained marks occupy less
than 1 MiB at the event cap, including spare queue capacity. The existing
one-day live book bound remains **46.5 MiB**, excluding allocator overhead.
Allocation estimates are
not exact process bounds and are per indicator pane. Current process/runtime
measurements for the four-day loader are in the
[fixed-marker and four-day receipt](evidence/orderflow-four-day-fixed-markers-2026-10-08.json).
The native load processed 4,192,385 executions into 113 signals, with 0.22 MiB
of summaries after loading, 643 ms of replay work distributed across messages
and a 5.9 ms largest ingestion/replay message. Whole-app peak working set was
407 MiB, alongside the preserved four-pane layout. These are one-run
measurements; the
[previous 24-hour receipt](evidence/orderflow-24h-resources-2026-10-08.json)
provides the historical baseline.

## Reproduce

Using an extracted official Binance USD-M aggTrades CSV:

```powershell
cargo run -p flowsurface-data --release --example orderflow_replay -- BTCUSDT-aggTrades-2026-10-07.csv default 1791379800000 1791403200000
```

Optional arguments: a legacy JSON configuration or `default`, a session
`[from_ms,to_ms)`, then price/quantity unit scales. Legacy tuning fields are
ignored. Include warm-up and five minutes of outcome tail. Input is capped at
128 MiB, two million executions and 90,000 seconds; CSV/archive files are not
committed. The harness calls production detection. Unit rescaling is diagnostic,
not a simulation of a different real market.

When an archive is unavailable, existing `orderflow_history` exports recorder
coverage through `ServerClient` and OS-keychain credentials. It uses bounded
sequential pages, rejects incomplete coverage, preserves ties and never writes
to the server.

Older [Selective replay](evidence/orderflow-btc-replay-2026-10-08.json),
[Balanced NY](evidence/orderflow-ny-session-2026-10-08.json) and
[runtime verification](evidence/orderflow-btc-verification-2026-10-08.json)
receipts describe earlier policies. Reproduce from their recorded commits;
old preset JSON does not restore the old detector in current code.

## Bybit and Hyperliquid later

The domain is venue-neutral. Runtime currently accepts Binance BTCUSDT through
existing `TickerInfo` and trade streams. Extend through `data::aggregation` and
recorder history with common quote-notional units and UTC seconds. Keep venue
readiness/gaps/deduplication separate, and define an explicit price reference.
Do not sum differently priced venue extrema or reinterpret a venue failure as
exhaustion. No new transport, database, GUI framework or crate is needed now.

Aggregation is feasible through the existing Binance/Bybit/Hyperliquid BTC feed.
The current layout already subscribes to their trades for other surfaces, so
subscriptions can be shared. A merged capped summary book can keep the same
per-second band and history bounds instead of retaining three full books.
Processing and historical transfer costs still grow with total execution count;
an exact overhead needs measurement on the implemented aggregate path.
Sequential recorder backfill, per-venue coverage/watermarks and reconnect
deduplication are needed. The Binance archive fallback cannot fill Bybit or
Hyperliquid coverage gaps. Quote-currency differences and venue price premiums
need an explicit common reference before combining bands. These correctness
rules are the main additional implementation work. Aggregation is not enabled
by the four-day-history or fixed-marker presentation change.
