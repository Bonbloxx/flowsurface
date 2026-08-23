import test from "node:test";
import assert from "node:assert/strict";
import {
  collectAll,
  normalizeBinance,
  normalizeBybit,
  normalizeHyperliquid,
  route,
} from "../src/worker.mjs";

function recordingDb() {
  const writes = [];
  return {
    writes,
    prepare() {
      return {
        bind(...values) {
          return { values };
        },
      };
    },
    async batch(statements) {
      writes.push(...statements.map((statement) => statement.values));
      return statements.map(() => ({ success: true }));
    },
  };
}

test("normalizes all three venues to one-sided USD OI", () => {
  const binance = normalizeBinance(
    { openInterest: "100", time: 1_700_000_012_345 },
    { markPrice: "50" },
  );
  assert.equal(binance.usdOpenInterest, 5_000);
  assert.equal(binance.bucketMs, 1_699_999_980_000);

  const bybit = normalizeBybit({
    retCode: 0,
    time: 1_700_000_012_345,
    result: { list: [{
      openInterest: "200",
      openInterestValue: "10000",
      singleOpenInterest: "100",
      singleOpenInterestValue: "5000",
      markPrice: "50",
    }] },
  });
  assert.equal(bybit.rawOpenInterest, 100);
  assert.equal(bybit.usdOpenInterest, 5_000);

  const hyperliquid = normalizeHyperliquid([
    { universe: [{ name: "ETH" }, { name: "BTC" }] },
    [{ openInterest: "2", markPx: "25" }, { openInterest: "100", markPx: "50" }],
  ], 1_700_000_012_345);
  assert.equal(hyperliquid.rawOpenInterest, 100);
  assert.equal(hyperliquid.usdOpenInterest, 5_000);
});

test("Bybit falls back from documented both-side values without doubling OI", () => {
  const value = normalizeBybit({
    retCode: 0,
    time: 1_700_000_012_345,
    result: { list: [{
      openInterest: "200",
      openInterestValue: "10000",
      markPrice: "50",
    }] },
  });
  assert.equal(value.rawOpenInterest, 100);
  assert.equal(value.usdOpenInterest, 5_000);
});

test("history endpoint is authenticated, ordered and paged", async () => {
  const rows = [
    { bucket_ms: 60_000, observed_at_ms: 61_000, usd_open_interest: 10 },
    { bucket_ms: 120_000, observed_at_ms: 121_000, usd_open_interest: 11 },
    { bucket_ms: 180_000, observed_at_ms: 181_000, usd_open_interest: 12 },
  ];
  const env = {
    READ_TOKEN: "secret",
    DB: {
      prepare() {
        return {
          bind() {
            return { all: async () => ({ results: rows }) };
          },
        };
      },
    },
  };
  const unauthorized = await route(
    new Request("https://worker.test/v1/open-interest?venue=binance&market=linear&symbol=BTCUSDT"),
    env,
  );
  assert.equal(unauthorized.status, 401);

  const missingSecret = await route(
    new Request(
      "https://worker.test/v1/open-interest?venue=binance&market=linear&symbol=BTCUSDT",
    ),
    { ...env, READ_TOKEN: undefined },
  );
  assert.equal(missingSecret.status, 401);

  const response = await route(new Request(
    "https://worker.test/v1/open-interest?venue=binance&market=linear&symbol=BTCUSDT&from=0&to=200000&limit=2",
    { headers: { authorization: "Bearer secret" } },
  ), env);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), {
    version: 1,
    venue: "binance",
    market: "linear",
    symbol: "BTCUSDT",
    interval_ms: 60_000,
    points: [[60_000, 61_000, 10], [120_000, 121_000, 11]],
    next_from: 180_000,
  });
});

test("scheduled collection leaves Binance to the VPS and keeps direct venues independent", async () => {
  const requested = [];
  const fetcher = async (url) => {
    requested.push(String(url));
    if (String(url).includes("bybit")) {
      return new Response(JSON.stringify({
        retCode: 0,
        time: 1_700_000_012_345,
        result: { list: [{
          singleOpenInterest: "100",
          singleOpenInterestValue: "5000",
          markPrice: "50",
        }] },
      }));
    }
    return new Response(JSON.stringify([
      { universe: [{ name: "BTC" }] },
      [{ openInterest: "100", markPx: "50" }],
    ]));
  };
  const DB = recordingDb();

  const samples = await collectAll({ DB }, 1_700_000_012_345, fetcher);

  assert.deepEqual(samples.map((value) => value.venue), ["bybit", "hyperliquid"]);
  assert.equal(DB.writes.length, 2);
  assert.equal(requested.some((url) => url.includes("binance.com")), false);
});

test("VPS ingest is source-locked, live-only, and recomputes USD OI", async () => {
  const now = 1_700_000_100_000;
  const DB = recordingDb();
  const env = { DB, INGEST_TOKEN: "ingest-secret" };
  const endpoint = "https://worker.test/v1/ingest/open-interest";

  const unauthorized = await route(new Request(endpoint, {
    method: "POST",
    body: JSON.stringify({}),
  }), env, fetch, now);
  assert.equal(unauthorized.status, 401);

  const payload = {
    version: 1,
    venue: "binance",
    market: "linear",
    symbol: "BTCUSDT",
    observed_at_ms: now - 1_234,
    raw_open_interest: "100.25",
    mark_price: "50.5",
    usd_open_interest: 1,
  };
  const accepted = await route(new Request(endpoint, {
    method: "POST",
    headers: {
      authorization: "Bearer ingest-secret",
      "content-type": "application/json",
    },
    body: JSON.stringify(payload),
  }), env, fetch, now);
  assert.equal(accepted.status, 201);
  assert.equal(DB.writes.length, 1);
  assert.deepEqual(DB.writes[0].slice(0, 4), [
    "binance",
    "linear",
    "BTCUSDT",
    Math.floor((now - 1_234) / 60_000) * 60_000,
  ]);
  assert.equal(DB.writes[0][5], 100.25);
  assert.equal(DB.writes[0][6], "base");
  assert.equal(DB.writes[0][7], 50.5);
  assert.equal(DB.writes[0][8], 100.25 * 50.5);

  const stale = await route(new Request(endpoint, {
    method: "POST",
    headers: {
      authorization: "Bearer ingest-secret",
      "content-type": "application/json",
    },
    body: JSON.stringify({ ...payload, observed_at_ms: now - 10 * 60_000 - 1 }),
  }), env, fetch, now);
  assert.equal(stale.status, 400);

  const wrongSource = await route(new Request(endpoint, {
    method: "POST",
    headers: {
      authorization: "Bearer ingest-secret",
      "content-type": "application/json",
    },
    body: JSON.stringify({ ...payload, venue: "bybit" }),
  }), env, fetch, now);
  assert.equal(wrongSource.status, 400);
  assert.equal(DB.writes.length, 1);
});

test("VPS ingest rejects oversized bodies before parsing", async () => {
  const response = await route(new Request(
    "https://worker.test/v1/ingest/open-interest",
    {
      method: "POST",
      headers: { authorization: "Bearer ingest-secret" },
      body: `{"padding":"${"x".repeat(4_096)}"}`,
    },
  ), { INGEST_TOKEN: "ingest-secret", DB: recordingDb() });

  assert.equal(response.status, 400);
  assert.match((await response.json()).error, /too large/);
});
