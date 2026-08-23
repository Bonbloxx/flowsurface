# Binance OI collector

This dependency-free Python process fetches the official Binance BTCUSDT
linear-perpetual OI and mark price, then sends the raw values to the protected
Worker ingest endpoint. The Worker validates recency and recomputes USD OI.

Production runs as a lingering systemd user timer. Keep the code, units, and
mode-0600 `.env` in `$HOME/flowsurface/oi-collector`; link the units
with `systemctl --user link`, then enable the timer. Logs go only to journald:

```sh
systemctl --user status flowsurface-oi-collector.timer
journalctl --user -u flowsurface-oi-collector.service
```

Run `python3 -m unittest -v` for network-free normalization tests. No Binance
or Python package credentials are required.
