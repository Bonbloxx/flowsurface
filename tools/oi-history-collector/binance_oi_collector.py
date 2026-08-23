#!/usr/bin/env python3
"""Collect one truthful Binance BTCUSDT OI snapshot and send it to the Worker."""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal, InvalidOperation
from typing import Any

BINANCE_OI_URL = "https://fapi.binance.com/fapi/v1/openInterest?symbol=BTCUSDT"
BINANCE_MARK_URL = "https://fapi.binance.com/fapi/v1/premiumIndex?symbol=BTCUSDT"
USER_AGENT = "flowsurface-oi-collector/1"
REQUEST_TIMEOUT_SECONDS = 10
MAX_RESPONSE_BYTES = 1_048_576
MAX_ATTEMPTS = 3


class CollectorError(RuntimeError):
    """Expected upstream, validation, or ingest failure."""


def _decimal(payload: dict[str, Any], field: str, *, positive: bool = False) -> Decimal:
    try:
        value = Decimal(str(payload[field]))
    except (KeyError, InvalidOperation, ValueError) as error:
        raise CollectorError(f"invalid {field}") from error
    if not value.is_finite() or value < 0 or (positive and value == 0):
        raise CollectorError(f"invalid {field}")
    return value


def _timestamp(payload: dict[str, Any], field: str) -> int:
    value = payload.get(field)
    if isinstance(value, bool):
        raise CollectorError(f"invalid {field}")
    try:
        timestamp = int(value)
    except (TypeError, ValueError) as error:
        raise CollectorError(f"invalid {field}") from error
    if timestamp <= 0 or timestamp > 9_007_199_254_740_991:
        raise CollectorError(f"invalid {field}")
    return timestamp


def build_payload(open_interest: dict[str, Any], premium_index: dict[str, Any]) -> dict[str, Any]:
    """Validate official Binance responses without inventing missing values."""

    raw_open_interest = _decimal(open_interest, "openInterest")
    mark_price = _decimal(premium_index, "markPrice", positive=True)
    return {
        "version": 1,
        "venue": "binance",
        "market": "linear",
        "symbol": "BTCUSDT",
        "observed_at_ms": _timestamp(open_interest, "time"),
        # Decimal strings preserve the upstream representation in transit.
        "raw_open_interest": str(raw_open_interest),
        "mark_price": str(mark_price),
    }


def _request_json(request: urllib.request.Request) -> Any:
    try:
        with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SECONDS) as response:
            body = response.read(MAX_RESPONSE_BYTES + 1)
            if len(body) > MAX_RESPONSE_BYTES:
                raise CollectorError("response body is too large")
            if response.status < 200 or response.status >= 300:
                raise CollectorError(f"request returned HTTP {response.status}")
    except urllib.error.HTTPError as error:
        raise CollectorError(f"request returned HTTP {error.code}") from error
    except urllib.error.URLError as error:
        raise CollectorError(f"request failed: {error.reason}") from error
    try:
        return json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise CollectorError("response was not valid JSON") from error


def _get_json(url: str) -> Any:
    return _request_json(urllib.request.Request(
        url,
        headers={"accept": "application/json", "user-agent": USER_AGENT},
    ))


def collect_payload() -> dict[str, Any]:
    # Fetch concurrently so the OI and mark observations have minimal skew.
    with ThreadPoolExecutor(max_workers=2, thread_name_prefix="binance-oi") as executor:
        oi_future = executor.submit(_get_json, BINANCE_OI_URL)
        mark_future = executor.submit(_get_json, BINANCE_MARK_URL)
        open_interest = oi_future.result()
        premium_index = mark_future.result()
    if not isinstance(open_interest, dict) or not isinstance(premium_index, dict):
        raise CollectorError("Binance response shape was invalid")
    return build_payload(open_interest, premium_index)


def post_payload(ingest_url: str, ingest_token: str, payload: dict[str, Any]) -> None:
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    request = urllib.request.Request(
        ingest_url,
        data=body,
        method="POST",
        headers={
            "accept": "application/json",
            "authorization": f"Bearer {ingest_token}",
            "content-type": "application/json",
            "user-agent": USER_AGENT,
        },
    )
    response = _request_json(request)
    if not isinstance(response, dict) or response.get("accepted") is not True:
        raise CollectorError("ingest response did not confirm acceptance")


def collect_once() -> dict[str, Any]:
    ingest_url = os.environ.get("FLOWSURFACE_OI_INGEST_URL", "").strip()
    ingest_token = os.environ.get("FLOWSURFACE_OI_INGEST_TOKEN", "")
    if not ingest_url.startswith("https://"):
        raise CollectorError("FLOWSURFACE_OI_INGEST_URL must be HTTPS")
    if not ingest_token:
        raise CollectorError("FLOWSURFACE_OI_INGEST_TOKEN is missing")

    payload = collect_payload()
    post_payload(ingest_url, ingest_token, payload)
    return payload


def main() -> int:
    for attempt in range(1, MAX_ATTEMPTS + 1):
        try:
            payload = collect_once()
            print(json.dumps({
                "event": "binance_oi_collected",
                "symbol": payload["symbol"],
                "observed_at_ms": payload["observed_at_ms"],
            }, separators=(",", ":")), flush=True)
            return 0
        except (CollectorError, TimeoutError, ValueError) as error:
            print(json.dumps({
                "event": "binance_oi_collection_failed",
                "attempt": attempt,
                "error": str(error),
            }, separators=(",", ":")), file=sys.stderr, flush=True)
            if attempt < MAX_ATTEMPTS:
                time.sleep(2 ** (attempt - 1))
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
