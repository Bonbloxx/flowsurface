import test from 'node:test';
import assert from 'node:assert/strict';
import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';
import { route } from '../src/worker.mjs';

const minute = 60_000, day = 86_400_000;
const schema = [1, 2, 3].map((n) => readFileSync(new URL(`../migrations/${[
  '', '0001_initial.sql', '0002_oi_definition.sql', '0003_history_days.sql',
][n]}`, import.meta.url), 'utf8'));
function fixture() {
  const db = new DatabaseSync(':memory:');
  db.exec(schema[0] + schema[1]);
  const insert = db.prepare('INSERT INTO oi_samples VALUES(?,?,?,?,?,?,?,?,?,?)');
  db.exec('BEGIN');
  for (const [venue, symbol] of [['binance', 'BTCUSDT'], ['bybit', 'BTCUSDT'], ['hyperliquid', 'BTC']]) {
    for (let i = 0; i < 16 * 1440; i++) {
      // Preserve sparse minutes and an entire missing day.
      if (i % 13 === 0 || Math.floor(i / 1440) === 5) continue;
      insert.run(venue, 'linear', symbol, i * minute, i * minute + 12345,
        91234.56789012345 + i / 1000, 'base', 82456.12345678901,
        7532712345.123456 + i, venue === 'bybit' && i < 1440 ? 'legacy' : 'venue_reported');
    }
  }
  db.exec('COMMIT');
  const before = db.prepare('SELECT * FROM oi_samples ORDER BY venue,market,symbol,bucket_ms').all();
  db.exec(schema[2]);
  assert.deepEqual(db.prepare('SELECT * FROM oi_samples ORDER BY venue,market,symbol,bucket_ms').all(), before);
  const DB = {
    prepare(sql) {
      return { bind(...params) {
        return { all: async () => ({ results: db.prepare(sql).all(...params) }) };
      } };
    },
  };
  return { db, DB };
}
const headers = { authorization: 'Bearer secret' };
async function page(DB, params, projected) {
  const url = 'https://worker.test/v1/open-interest?' + new URLSearchParams({
    venue: 'binance', market: 'linear', symbol: 'BTCUSDT', ...params,
  });
  const response = await route(new Request(url, { headers }), {
    DB, READ_TOKEN: 'secret', HISTORY_DAYS: projected ? 'v1' : undefined,
  });
  assert.equal(response.status, 200);
  return response.json();
}
test('daily history preserves exact raw/USD values, gaps, boundaries and pagination', async () => {
  const { db, DB } = fixture();
  try {
    for (const venue of ['binance', 'bybit', 'hyperliquid']) {
      for (const [from, to, limit] of [[0, 16 * day, 1000], [0, 16 * day, 10000],
        [minute + 1, 10 * day - 1, 20000], [5 * day, 6 * day - 1, 10000],
        [15 * day + 3 * minute, 20 * day, 10000], [0, 16 * day, 1]]) {
        for (const include_raw of ['0', '1']) {
          const params = { venue, symbol: venue === 'hyperliquid' ? 'BTC' : 'BTCUSDT',
            from, to, limit, include_raw };
          assert.deepEqual(await page(DB, params, true), await page(DB, params, false));
        }
      }
    }
    let from = 0, pages = [], expected = [];
    do {
      const result = await page(DB, { from, to: 16 * day, limit: 1000, include_raw: '1' }, true);
      pages.push(...result.observations); from = result.next_from;
    } while (from !== null);
    expected = (await page(DB, { from: 0, to: 16 * day, limit: 20000, include_raw: '1' }, false)).observations;
    assert.deepEqual(pages, expected);
  } finally { db.close(); }
});
test('projection follows inserts, revisions, moved keys, deletions and transaction rollback', async () => {
  const { db, DB } = fixture();
  try {
    const params = { from: 0, to: 20 * day, limit: 20000, include_raw: '1' };
    const equal = async () => assert.deepEqual(await page(DB, params, true), await page(DB, params, false));
    db.prepare('UPDATE oi_samples SET raw_open_interest=?,usd_open_interest=?,observed_at_ms=observed_at_ms+1 WHERE venue=? AND bucket_ms=?')
      .run(0.12345678901234568, Number.MIN_VALUE, 'binance', minute);
    db.prepare('UPDATE oi_samples SET bucket_ms=? WHERE venue=? AND bucket_ms=?')
      .run(19 * day, 'binance', 2 * minute);
    db.prepare('DELETE FROM oi_samples WHERE venue=? AND bucket_ms=?').run('binance', 3 * minute);
    db.prepare('INSERT INTO oi_samples VALUES(?,?,?,?,?,?,?,?,?,?)').run('binance', 'linear', 'BTCUSDT',
      5 * day, 5 * day + 1234, 1.7976931348623157e308, 'base', 1, 1.7976931348623157e308, 'venue_reported');
    await equal();
    const before = await page(DB, params, true);
    db.exec('BEGIN; UPDATE oi_samples SET raw_open_interest=0 WHERE venue=\'binance\'; ROLLBACK;');
    assert.deepEqual(await page(DB, params, true), before);
    await equal();
  } finally { db.close(); }
});
test('projection failure falls back to original samples and authentication remains required', async () => {
  const { db, DB } = fixture();
  try {
    const params = { from: 0, to: 16 * day, limit: 10000, include_raw: '1' };
    const expected = await page(DB, params, false);
    db.exec('DROP TABLE oi_history_days');
    assert.deepEqual(await page(DB, params, true), expected);
    const response = await route(new Request('https://worker.test/v1/open-interest?venue=binance&market=linear&symbol=BTCUSDT'),
      { DB, READ_TOKEN: 'secret', HISTORY_DAYS: 'v1' });
    assert.equal(response.status, 401);
  } finally { db.close(); }
});
