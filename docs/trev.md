# TRev chart

Select **TRev Chart** in the pane picker, then choose a market. Open the pane's
settings or its `TRev continuation/reversal` header button to configure it.

- Continuation and reversal are independent distances in exchange minimum-price
  ticks. Both accept integers from 1 through 100,000. Neither must be larger.
- Enter both distances and press **Apply thresholds** or Enter. Only that pane
  rebuilds. Defaults are 200 continuation ticks and 300 reversal ticks.
- Choose candles, bid/ask footprint, volume footprint, or delta footprint.
- **Ticks per footprint row** aggregates the display without changing the input
  price grid, candle boundaries, stored quantities, or raw footprint levels.
- Minimum lifetime defaults to zero. A nonzero value requires both elapsed time
  and a threshold-crossing execution before a bar can close.

## Construction

The first execution anchors the initial bar. Until direction is established,
the continuation distance applies in either direction. Thereafter each bar
opens at the previous bar's actual closing price:

| Previous direction | Continuation | Reversal |
| --- | --- | --- |
| Up | Open + continuation ticks | Open - reversal ticks |
| Down | Open - continuation ticks | Open + reversal ticks |

Reaching a threshold closes the bar and establishes the next direction. The
triggering execution belongs entirely to the closing bar. The next developing
bar starts with zero executions. Bar OHLC, side quantities and footprint cells
come from the actual executions. A print that jumps across a threshold closes
at its real price; it does not invent intermediate prints or repeated volume.
With lifetime normalization, real excursions before closure remain in the wick.

This is a documented TRev implementation, not a claim of exact Exocharts parity.
Public Exocharts documentation does not fully specify startup, execution
ownership and all gap/normalization edge cases.

## History and isolation

The chart has a distinct picker entry, configuration, aggregation state, history
request identity and versioned `trev_state` checkpoint. Existing Renko, time,
footprint and TPO construction rules are unchanged. Visual-settings broadcasts
cannot cross between TRev and another chart type. Each pane owns its mutable
state; the existing normalized exchange subscriptions are still shared.

Like current Renko, TRev constructs its price bars from one market/venue. It
supports the same trade-history modes and normalized exchange executions, and
adds independent footprint row aggregation. It does not advertise multi-venue
bar construction. Existing trade-history indicators can retain their supported
venue selection. CVD currently rejects tick-indexed charts, so TRev starts with
Volume and does not expose that unsupported CVD subplot.

History uses the existing connector and Arrow protocol. It replays complete,
ordered execution pages, buffering live data until the history/live seam is
resolved. Server history opts into `stable_order=true`, with source insertion
order retained for executions sharing a millisecond. The recorder must confirm
`x-flowsurface-trade-order: timestamp-insertion-v1`; an older recorder is rejected
instead of constructing a price path from unstable timestamp ties. Other charts
keep their existing request behavior. Boundary duplicates are matched by multiplicity, including multiple
identical prints in the same millisecond. Coverage gaps start new segments;
confirmed history is not rewritten to bridge unavailable executions.

The retained window is four hours, with at most 10,000 completed bars and a
250,000-row budget for retained completed history. Whole older bars expire;
the developing bar keeps its exact anchor and observed quantities. Snapshots
preserve every retained footprint row, side counts and replay boundary.
Malformed or incompatible market/configuration checkpoints are rejected. An
expired offline checkpoint starts a fresh replay window.

## Verification

Run `cargo test --workspace`, `cargo fmt --all -- --check`, and
`cargo clippy --workspace --all-targets --all-features -- -D warnings`.

`cargo run --example trev_native_qa -- target/trev-native-qa` renders the actual
native pane picker, settings, four TRev displays, regrouping, saved-state restore
and the other candle-family charts using an offline synthetic tape. It creates
an offscreen native window and exports RGBA screenshots with JSON dimensions.
It starts no exchange subscriptions and does not load or modify the user's
profile. The restore stage asserts exact OHLC, volume, footprint and continuation
state parity.

For an independent exact-data audit, export coverage-proven executions with
`cargo run --release --example orderflow_history -- URL FROM_MS TO_MS tape.csv ordered`,
then run `cargo run --release -p flowsurface-data --example trev_verify -- tape.csv OUTPUT`
and `python scripts/verify-trev.py tape.csv OUTPUT/default.json`. The verifier
compares every OHLC value, side quantity, execution count, native footprint cell
and replay boundary using a separate integer/Decimal implementation. The Rust
probe also checks batch independence and bounded million-execution stress.

Set `FLOWSURFACE_TREV_PERF=1` with info logging to measure native TRev draw CPU
times (bounded 512-sample window). It is disabled in ordinary operation.
