# Open-interest history

Flowsurface uses an optional always-on collector for truthful one-minute OI
history. The desktop app remains a consumer and local cache, so collection does
not depend on a pane, the app, or the user's PC being online.

```
Binance public REST -> VPS systemd user timer -> authenticated Worker ingest
Bybit / Hyperliquid public APIs ---------> Worker cron (once per minute)
                                           -> D1 source-of-record
                                           -> GET /v1/open-interest
                                           -> native connector -> local cache
                                           -> OI indicator
```

The collector stores the venue, market, symbol, observation time, raw OI and
unit, mark price, normalized USD OI, and the venue-definition marker. Binance
and Hyperliquid expose their published OI directly. Bybit's published
`openInterest` / `openInterestValue` fields are explicitly both-side values, so
the combined readout uses those values rather than its separate single-side
fields. This matches the venue totals shown by order-flow platforms while the
tooltip calls the result "venue-reported" instead of implying identical venue
methodology. Legacy Bybit rows retain an explicit marker and are doubled once
on read; new rows are stored in the venue-published definition.

The native client keeps the newest
observation in each minute, so collected one-minute observations supersede
coarser venue history without hiding a newer direct venue snapshot. Exchange
history fills older ranges and its completed-interval timestamps are normalized
one millisecond inside the interval they summarize. Current snapshots keep
their exact observation time. This removes the old blanket visual shift and
places both the current and previous one-minute candles correctly. The local
cache is the offline fallback.
Missing values are never interpolated; aggregate candles require every selected
venue.

The service is optional. Its URL and read token are configured separately from
the trade-history server, and the token stays in the OS keychain. Binance uses
an independent, ingest-only secret on the VPS because Binance rejects
Cloudflare egress. Bybit and Hyperliquid remain direct Worker sources. The VPS
timer uses systemd lingering, so collection continues while the desktop app
and the user's PC are off. A missed minute stays missing; restart never
interpolates or fabricates it.

The reference deployment records BTC linear perpetuals only: 1,440 snapshots
per venue per day. Additional markets must be explicitly added so cost,
storage, and exchange rate usage remain bounded.

Worker instructions live in `tools/oi-history-worker/README.md`; the Binance
process and user units live in `tools/oi-history-collector/`.
