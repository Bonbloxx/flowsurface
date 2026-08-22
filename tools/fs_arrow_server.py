#!/usr/bin/env python3
"""Flowsurface trade-history arrow server (reference implementation).

Implements the contract expected by ``src/connector/client.rs``:

    GET /trades.arrow?venue=binance&market=linear&symbol=btcusdt&from=<ms>&to=<ms>&limit=<n>
        -> application/vnd.apache.arrow.stream
           schema: ts:int64 (ms UTC), price:double, qty:double, is_sell:boolean

    GET /pairs
        -> {"pairs": [{"ticker": "binancelinear:btcusdt", "earliest": <ms|null>}, ...]}

Data source: Binance daily aggTrades archives (data.binance.vision). Zips the
app already downloaded under %APPDATA%/flowsurface/market_data are reused;
missing days are downloaded lazily on first request and every parsed day is
cached as an Arrow file next to its zip, so repeat requests are instant.

Usage (Python 3.11+, `pip install pyarrow`):
    python fs_arrow_server.py serve --port 8080
    python fs_arrow_server.py prefetch --venue binance --market linear --symbols BTCUSDT --days 30
    set FS_TOKEN=... to require "Authorization: Bearer <token>" on all requests
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import sys
import threading
import time
import urllib.request
import zipfile
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pacsv
import pyarrow.ipc as paipc

DAY_MS = 86_400_000
APPDATA = os.environ.get("APPDATA", "")
ARCHIVE_BASES = {
    ("binance", "linear"): "https://data.binance.vision/data/futures/um/daily/aggTrades",
    ("binance", "spot"): "https://data.binance.vision/data/spot/daily/aggTrades",
}
# Zips the flowsurface app itself has downloaded.
APP_ZIP_ROOTS = {
    ("binance", "linear"): [
        os.path.join(APPDATA, "flowsurface", "market_data", "binance",
                     "data", "futures", "um", "daily", "aggTrades"),
    ],
    ("binance", "spot"): [
        os.path.join(APPDATA, "flowsurface", "market_data", "binance",
                     "data", "spot", "daily", "aggTrades"),
    ],
}
STORE_ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "store")
MISSING_TTL_S = 3600  # re-attempt a 404 archive after one hour

_locks: dict[str, threading.Lock] = {}
_locks_guard = threading.Lock()
_missing: dict[tuple, float] = {}
_missing_guard = threading.Lock()


def _lock_for(key: str) -> threading.Lock:
    with _locks_guard:
        return _locks.setdefault(key, threading.Lock())


def _mark_missing(key: tuple) -> None:
    with _missing_guard:
        _missing[key] = time.time()


def _is_missing(key: tuple) -> bool:
    with _missing_guard:
        seen = _missing.get(key)
        return seen is not None and time.time() - seen < MISSING_TTL_S


def _utc_today() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%d")


def _zip_candidates(venue: str, market: str, symbol: str, day: str):
    """All plausible paths for one archive day, app cache first."""
    upper = symbol.upper()
    lower = symbol.lower()
    paths = []
    for root in APP_ZIP_ROOTS.get((venue, market), []):
        paths.append(os.path.join(root, upper, f"{upper}-aggTrades-{day}.zip"))
    paths.append(os.path.join(STORE_ROOT, venue, market, lower,
                              f"{upper}-aggTrades-{day}.zip"))
    return paths


def ensure_zip(venue: str, market: str, symbol: str, day: str):
    """Return a local zip path for the day, downloading it once if needed."""
    for path in _zip_candidates(venue, market, symbol, day):
        if os.path.exists(path):
            return path
    key = (venue, market, symbol.lower(), day)
    if _is_missing(key) or day >= _utc_today():
        return None  # unpublished / not yet attempted
    base = ARCHIVE_BASES.get((venue, market))
    if base is None:
        return None
    url = f"{base}/{symbol.upper()}/{symbol.upper()}-aggTrades-{day}.zip"
    dest = _zip_candidates(venue, market, symbol, day)[-1]
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    tmp = dest + f".{os.getpid()}.part"
    try:
        print(f"[ingest] downloading {url}", flush=True)
        with urllib.request.urlopen(url, timeout=120) as resp, open(tmp, "wb") as fh:
            while chunk := resp.read(1 << 20):
                fh.write(chunk)
        os.replace(tmp, dest)
        return dest
    except Exception as exc:  # noqa: BLE001 - any failure means "no data today"
        print(f"[ingest] unavailable ({exc})", flush=True)
        _mark_missing(key)
        try:
            os.remove(tmp)
        except OSError:
            pass
        return None


def _table_from_csv(fh) -> pa.Table:
    table = pacsv.read_csv(fh)
    cols = {name.lower(): name for name in table.column_names}
    ts = cols["transact_time"]
    price = cols["price"]
    qty = cols["quantity"]
    sell = cols["is_buyer_maker"]

    out = pa.table({
        "ts": pc.cast(table[ts], pa.int64()),
        "price": pc.cast(table[price], pa.float64()),
        "qty": pc.cast(table[qty], pa.float64()),
        "is_sell": pc.cast(table[sell], pa.bool_()),
    })
    return out.sort_by([("ts", "ascending")])


def load_day_table(venue: str, market: str, symbol: str, day: str) -> pa.Table | None:
    """Parsed Arrow table for one archive day (converted at most once)."""
    key = f"{venue}:{market}:{symbol}:{day}"
    with _lock_for(key):
        zip_path = ensure_zip(venue, market, symbol, day)
        if zip_path is None:
            return None
        cache_path = zip_path[: -4] + ".arrow"
        if os.path.exists(cache_path) and \
                os.path.getmtime(cache_path) >= os.path.getmtime(zip_path):
            with pa.memory_map(cache_path, "rb") as src:
                return paipc.RecordBatchFileReader(src).read_all()

        table = None
        with zipfile.ZipFile(zip_path) as zf:
            for entry in zf.namelist():
                if not entry.lower().endswith(".csv"):
                    continue
                with zf.open(entry) as fh:
                    table = _table_from_csv(fh)
                break
        if table is None or table.num_rows == 0:
            return None

        tmp = cache_path + ".tmp"
        with open(tmp, "wb") as sink:
            with paipc.new_file(sink, table.schema) as writer:
                writer.write_table(table)
        os.replace(tmp, cache_path)
        print(f"[ingest] converted {os.path.basename(zip_path)} "
              f"({table.num_rows} trades)", flush=True)
        return table


def query_trades(venue: str, market: str, symbol: str,
                 frm: int, to: int, limit: int) -> pa.Table:
    """Rows in [frm, to] ascending, capped at `limit`."""
    first_day = datetime.fromtimestamp(max(frm, 0) / 1000, timezone.utc) \
        .strftime("%Y-%m-%d")
    last_day = datetime.fromtimestamp(to / 1000, timezone.utc).strftime("%Y-%m-%d")
    parts: list[pa.Table] = []
    total_rows = 0

    day = datetime.strptime(first_day, "%Y-%m-%d")
    last = datetime.strptime(last_day, "%Y-%m-%d")
    while day <= last:
        stamp = day.strftime("%Y-%m-%d")
        table = load_day_table(venue, market, symbol, stamp)
        if table is not None and table.num_rows:
            masked = table.filter(
                pc.and_(pc.field("ts") >= pa.scalar(frm),
                        pc.field("ts") <= pa.scalar(to)))
            if masked.num_rows:
                parts.append(masked)
                total_rows += masked.num_rows
                if total_rows >= limit:
                    break
        day += timedelta(days=1)

    if not parts:
        return pa.table({"ts": pa.array([], pa.int64()),
                         "price": pa.array([], pa.float64()),
                         "qty": pa.array([], pa.float64()),
                         "is_sell": pa.array([], pa.bool_())})
    merged = pa.concat_tables(parts)
    return merged.slice(0, min(limit, merged.num_rows))


def available_tickers():
    """(venue, market, symbol) tuples that have at least one local archive."""
    found = []
    for (venue, market), roots in APP_ZIP_ROOTS.items():
        for root in roots:
            if not os.path.isdir(root):
                continue
            for entry in os.listdir(root):
                if os.path.isdir(os.path.join(root, entry)):
                    found.append((venue, market, entry.lower()))
    store_base = STORE_ROOT
    if os.path.isdir(store_base):
        for venue in os.listdir(store_base):
            vdir = os.path.join(store_base, venue)
            if not os.path.isdir(vdir):
                continue
            for market in os.listdir(vdir):
                mdir = os.path.join(vdir, market)
                if not os.path.isdir(mdir):
                    continue
                for entry in os.listdir(mdir):
                    if os.path.isdir(os.path.join(mdir, entry)):
                        found.append((venue, market, entry.lower()))
    return sorted(set(found))


def pairs_payload():
    pairs = []
    for venue, market, symbol in available_tickers():
        earliest = None
        days = []
        for root in APP_ZIP_ROOTS.get((venue, market), []):
            sym_dir = os.path.join(root, symbol.upper())
            if os.path.isdir(sym_dir):
                days.extend(_days_in(sym_dir))
        days.extend(_days_in(os.path.join(STORE_ROOT, venue, market, symbol)))
        if days:
            start = datetime.strptime(min(days), "%Y-%m-%d") \
                .replace(tzinfo=timezone.utc)
            earliest = int(start.timestamp() * 1000)
        pairs.append({
            "ticker": f"{venue}{market}:{symbol}",
            "earliest": earliest,
        })
    return {"pairs": pairs}


def _days_in(directory: str):
    if not os.path.isdir(directory):
        return []
    days = []
    for name in os.listdir(directory):
        match = re.search(r"(\d{4}-\d{2}-\d{2})\.zip$", name)
        if match:
            days.append(match.group(1))
    return days


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _authorized(self) -> bool:
        token = os.environ.get("FS_TOKEN")
        if not token:
            return True
        header = self.headers.get("Authorization", "")
        return header == f"Bearer {token}"

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 - stdlib naming
        parsed = urlparse(self.path)
        try:
            if not self._authorized():
                self._send(401, b"unauthorized", "text/plain")
                return
            if parsed.path == "/pairs":
                body = json.dumps(pairs_payload()).encode()
                self._send(200, body, "application/json")
                return
            if parsed.path == "/trades.arrow":
                qs = parse_qs(parsed.query)
                venue = qs.get("venue", [""])[0].lower()
                market = qs.get("market", [""])[0].lower()
                symbol = qs.get("symbol", [""])[0].lower()
                frm = int(qs.get("from", ["0"])[0])
                to = int(qs.get("to", ["0"])[0])
                limit = min(int(qs.get("limit", ["400000"])[0]), 2_000_000)
                started = time.time()
                table = query_trades(venue, market, symbol, frm, to, limit)
                sink = io.BytesIO()
                with paipc.new_stream(sink, table.schema) as writer:
                    writer.write_table(table)
                print(f"[serve] {venue}/{market}/{symbol} "
                      f"{frm}-{to}: {table.num_rows} rows "
                      f"in {time.time() - started:.2f}s", flush=True)
                self._send(200, sink.getvalue(),
                           "application/vnd.apache.arrow.stream")
                return
            self._send(404, b"not found", "text/plain")
        except Exception as exc:  # noqa: BLE001 - report to client
            print(f"[error] {self.path}: {exc}", flush=True)
            self._send(500, str(exc).encode(), "text/plain")

    def log_message(self, *_args):  # silence default per-request noise
        pass


def cmd_serve(args) -> None:
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"listening on http://{args.host}:{args.port} "
          f"(store: {STORE_ROOT})", flush=True)
    server.serve_forever()


def cmd_prefetch(args) -> None:
    latest = datetime.strptime(_utc_today(), "%Y-%m-%d")
    for offset in range(1, args.days + 1):
        day = (latest - timedelta(days=offset)).strftime("%Y-%m-%d")
        for symbol in args.symbols:
            table = load_day_table(args.venue, args.market, symbol, day)
            rows = 0 if table is None else table.num_rows
            print(f"[prefetch] {args.market} {symbol} {day}: {rows} rows",
                  flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    serve = sub.add_parser("serve")
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=8080)
    serve.set_defaults(func=cmd_serve)

    fetch = sub.add_parser("prefetch")
    fetch.add_argument("--venue", default="binance")
    fetch.add_argument("--market", default="linear")
    fetch.add_argument("--symbols", nargs="+", default=["BTCUSDT"])
    fetch.add_argument("--days", type=int, default=30)
    fetch.set_defaults(func=cmd_prefetch)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
