"""Independent integer/Decimal oracle for a TRev snapshot and ordered execution CSV.

Usage: python scripts/verify-trev.py trades.csv snapshot.json [report.json]
The snapshot may be a plain TRev checkpoint or an application's saved-state.json.
"""
import csv
import json
import sys
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path


def checkpoint(value):
    if isinstance(value, dict):
        if {"version", "bars", "state", "step", "ticker"} <= value.keys():
            return value
        for key, child in value.items():
            if key == "trev_state" and child:
                return child
            found = checkpoint(child)
            if found:
                return found
    elif isinstance(value, list):
        for child in value:
            found = checkpoint(child)
            if found:
                return found
    return None


def atomic(value, scale):
    return int((Decimal(value) * scale).to_integral_value(rounding=ROUND_HALF_UP))


def verify(csv_path, snapshot_path):
    snapshot = checkpoint(json.loads(Path(snapshot_path).read_text(encoding="utf-8-sig")))
    assert snapshot is not None, "TRev checkpoint required"
    config = snapshot["state"]["config"]
    step = snapshot["step"]["units"]
    end = snapshot["state"]["last_time"]
    direction = None
    bars = []
    accepted = []
    before = -1
    boundary = [(t["time"], t["price"]["units"], t["qty"]["units"], t["is_sell"])
                for t in snapshot["state"]["last_trades"]]
    boundary_seen = 0
    unobserved_final_ms = 0

    def empty(time, price):
        return dict(time=time, open=price, high=price, low=price, close=price,
                    buy=0, sell=0, count=0, rows={})

    for fields in csv.reader(Path(csv_path).open(newline="", encoding="utf-8-sig")):
        time = int(fields[5])
        assert time >= before, "execution CSV is not ordered"
        before = time
        if time > end:
            break
        price = atomic(fields[1], 10**11)
        price = ((price + step//2)//step)*step
        qty = atomic(fields[2], 10**8)
        sell = fields[6].lower() == "true"
        if time == end:
            # A live snapshot may end partway through a millisecond. Prove its
            # exact source-order prefix before excluding the unobserved suffix.
            if boundary_seen >= len(boundary):
                unobserved_final_ms += 1
                continue
            assert (time, price, qty, sell) == boundary[boundary_seen], \
                "live final-millisecond boundary is not a prefix of recorder executions"
            boundary_seen += 1
        accepted.append((time, price, qty, sell))
        if not bars:
            bars.append(empty(time, price))
        bar = bars[-1]
        bar["high"] = max(bar["high"], price)
        bar["low"] = min(bar["low"], price)
        bar["close"] = price
        bar["count"] += 1
        side = "sell" if sell else "buy"
        bar[side] += qty
        row = bar["rows"].setdefault(price, dict(buy=0, sell=0, first=time,
                                                last=time, buys=0, sells=0))
        row[side] += qty
        row["sells" if sell else "buys"] += 1
        row["first"] = min(row["first"], time)
        row["last"] = max(row["last"], time)
        if time - bar["time"] < config["normalization_ms"]:
            continue
        displacement = price - bar["open"]
        trend, reverse = (config[key] * step for key in ("continuation_ticks", "reversal_ticks"))
        next_direction = None
        if direction is None:
            if abs(displacement) >= trend:
                next_direction = "Up" if displacement > 0 else "Down"
        elif direction == "Up":
            if displacement >= trend:
                next_direction = "Up"
            elif displacement <= -reverse:
                next_direction = "Down"
        elif displacement <= -trend:
            next_direction = "Down"
        elif displacement >= reverse:
            next_direction = "Up"
        if next_direction:
            direction = next_direction
            bars.append(empty(time, price))

    assert boundary_seen == len(boundary), "recorder export is missing observed boundary executions"
    # Apply the documented bounded-history policy independently of Rust.
    while len(bars) > 1 and (bars[0]["time"] < end - 14_400_000 or len(bars) > 10_001):
        bars.pop(0)
    row_count = sum(len(bar["rows"]) for bar in bars)
    while len(bars) > 1 and row_count > 250_000:
        row_count -= len(bars.pop(0)["rows"])
    expected = []
    for bar in bars:
        value = dict(bar)
        for key in ("open", "high", "low", "close", "buy", "sell"):
            value[key] = {"units": value[key]}
        value["rows"] = [[{"units": price}, {
            key: {"units": qty} if key in ("buy", "sell") else qty
            for key, qty in row.items()}] for price, row in sorted(bar["rows"].items())]
        expected.append(value)
    assert len(expected) == len(snapshot["bars"]), ("bar count", len(expected), len(snapshot["bars"]))
    for index, (actual, reference) in enumerate(zip(snapshot["bars"], expected)):
        for field in reference:
            assert actual[field] == reference[field], f"bar {index}, field {field} differs from independent execution replay"
    assert snapshot["state"]["direction"] == direction, "continuation direction differs"
    last_boundary = [dict(time=t, price={"units": p}, qty={"units": q}, is_sell=s)
                     for t, p, q, s in accepted if t == end]
    assert snapshot["state"]["last_trades"] == last_boundary, "last-millisecond execution multiplicity/order differs"
    return dict(input_executions=len(accepted), bars=len(expected), rows=row_count,
                buy_units=sum(bar["buy"]["units"] for bar in expected),
                sell_units=sum(bar["sell"]["units"] for bar in expected),
                unobserved_final_ms_executions=unobserved_final_ms,
                through_ms=end, config=config, exact=True)


if __name__ == "__main__":
    report = verify(sys.argv[1], sys.argv[2])
    result = json.dumps(report, indent=2)
    if len(sys.argv) > 3:
        Path(sys.argv[3]).write_text(result)
    print(result)
