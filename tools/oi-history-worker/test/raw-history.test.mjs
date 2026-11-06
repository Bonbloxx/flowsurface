import test from "node:test";
import assert from "node:assert/strict";
import { route } from "../src/worker.mjs";

test("raw OI is opt-in and preserves original observation time and units", async () => {
  const row = { bucket_ms: 60000, observed_at_ms: 61567, usd_open_interest: 5000,
    raw_open_interest: 100, raw_unit: "base", mark_price: 50, oi_definition: "venue_reported" };
  const env = { READ_TOKEN: "secret", DB: { prepare(sql) {
    assert.match(sql, /raw_open_interest/);
    return { bind() { return { all: async () => ({results: [row]}) }; } };
  } } };
  const url = "https://worker.test/v1/open-interest?venue=binance&market=linear&symbol=BTCUSDT";
  const headers = { authorization: "Bearer secret" };
  const old = await (await route(new Request(url, {headers}), env)).json();
  assert.deepEqual(old.points, [[60000, 61567, 5000]]);
  assert.equal(old.observations, undefined);
  const raw = await (await route(new Request(url + "&include_raw=1", {headers}), env)).json();
  assert.deepEqual(raw.points, old.points);
  assert.equal(raw.raw_version, 1);
  assert.deepEqual(raw.observations, [{bucket_ms: 60000, observed_at_ms: 61567,
    raw_open_interest: 100, raw_unit: "base", mark_price: 50,
    usd_open_interest: 5000, definition: "venue_reported"}]);
});
