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
unit, mark price, and normalized USD OI. The native client keeps the newest
observation in each minute, so collected one-minute observations supersede
coarser venue history without hiding a newer direct venue snapshot. Exchange
history fills older ranges, and the local cache is the offline fallback.
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
