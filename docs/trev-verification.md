# TRev verification — 2026-11-06

The release executable was installed at `target/release/flowsurface.exe` and
launched with the normal profile. Its SHA-256 matches the tested candidate:
`163b423362607d37dc80741f32b5de40842fee30ab37da26025820debf3c4cd8`.
Existing layouts, chart configuration, theme, units, network, sidebar, audio,
timezone and active layout were preserved. A clean-exit profile and executable
backup are in `target/trev-release-check/activation-20261106-122751/`.

## Exact data

- Independent integer/Decimal replay checked all OHLC values, buy/sell
  quantities, execution counts, native-price footprint quantities/counts/times,
  continuation direction and the execution boundary.
- A 22,655-execution covered Binance BTCUSDT recorder tape passed three
  construction configurations, including reversed threshold ratios and minimum
  lifetime. The before/after optimization snapshots are byte-identical.
- Four actual native panes (candles, bid/ask, volume and delta) passed the same
  audit after history catch-up: 1,023 executions, 139 bars and 758 native rows
  per pane. After app restart and catch-up, all four passed again with 2,405
  observed executions, 311 bars and 1,764 native rows per pane.
- A live checkpoint can stop partway through its final millisecond. The audit
  first verifies that its boundary executions are an exact ordered prefix of
  recorder executions, then replays only the observed prefix. In the restart
  case, one further execution in that millisecond had not been observed yet.
  All earlier milliseconds are compared in full. Deliberately corrupted
  footprint quantities and boundary sequences were rejected by the verifier.
- Twelve native QA stages covered picker, settings, four displays, footprint
  regrouping, checkpoint restore and candles/footprint/Renko/TPO regressions.
- Desktop: 474 tests passed, 16 ignored; formatting and strict all-target,
  all-feature Clippy passed. Recorder: 26 tests passed, two ignored; formatting
  and strict all-target Clippy passed.

These checks establish exactness against the covered recorder executions used
for verification. The construction rules are documented in [trev.md](trev.md);
they do not assert exact Exocharts parity.

## Performance

Measurements are from a release build on this workstation, not service limits.

| Probe | Result |
| --- | --- |
| Single execution into a 20,000-row footprint, p99 | 54.6 µs before; 0.1 µs after (4,000 samples) |
| Replay 22,655 executions with default thresholds | 1.20 ms |
| One-million-execution stress | 48.4 ms; 1,250 retained bars, 249,899 rows |
| Native TRev draw CPU, p95 in sampled live windows | About 0.07–0.09 ms |
| Four live native panes over 71 seconds | 6.33% of one CPU core; 248 MiB working set |
| Memory growth during that interval | 0.46 MiB working set; 0.41 MiB private |

TRev caches its retained row count and updates footprint POC incrementally.
Candles skip unnecessary POC work, and TRev no longer duplicates executions in
the generic raw-trade buffer. Construction and stored quantities are unchanged.

## Recorder deployment boundary

Live testing exposed unstable ordering of recorder executions with identical
millisecond timestamps. TRev now explicitly requests source insertion order;
other chart requests retain their prior behavior. The client rejects a recorder
that does not advertise the required capability.

The Linux recorder candidate passed 24/24 private HTTP history requests on a
26-million-row fixture while all three live feeds advanced. Repeated payloads
were byte-identical, peak process memory was about 436 MiB, and there were no
OOM or flush errors. The production PID and executable remained unchanged.
The stable-order regression also passed after an on-disk checkpoint, complete
storage close and reopen, preserving all 60,000 test executions.

**The recorder update was deployed after explicit approval.** The running Linux
process (PID 2903796) matches candidate SHA-256
`68d111a48ce8e666d865d99d4b23092b87144f81de9b63c1cb3bf8ad5ae40361`.
The previous process drained accepted trades and exited cleanly. The old binary
and complete stopped database were backed up before replacement; configuration,
authentication and TLS files retain their original hashes.

Independent read-only comparison of closed before/after database snapshots
verified all **81,654,649 retained pre-existing executions**, including duplicate
counts, exact timestamp/price/quantity/side records and capture intervals.
Another 10,630 records expired under the existing 30-day retention policy.
Six production history pages passed a separate exact comparison against
2,400,000 original database records. Repeated ordered production queries returned
identical payloads; legacy requests kept their existing ordering contract.

The installed desktop executable was then exercised in an isolated four-pane
profile using the production recorder and default 200/300 thresholds. All four
TRev displays matched the independent replay exactly: 2,600 observed executions,
nine bars and 1,438 native footprint rows per pane, through timestamp
1793961530124. All four history/live catch-ups completed. The normal Layout 1
profile, preferences and window geometry were checked again and are unchanged.

Observed restart capture gaps were 18.833 seconds for Binance, 17.702 seconds
for Bybit and 18.995 seconds for Hyperliquid. These gaps remain explicit in
coverage; no missing executions were invented. Fresh post-restart coverage is
complete, all three feeds advance, the health timer is active and the service
has had no automatic restarts or recording flush/OOM failures.

Production activation receipts, database snapshots and rollback material are
under the recorder's
`verification-20261106-trev-ordering/activation-20261106T103500Z/` directory.
Local copies of the integrity, HTTP-value, feed-health and independent native
replay reports use the `production-*` prefix in the evidence directory below.

Detailed captures, tape exports, independent reports, logs and sealed candidates
are under `target/trev-release-check/`.
