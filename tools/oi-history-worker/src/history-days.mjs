// Daily read projections remain in the same D1 transaction as oi_samples.
// Recent, small requests keep the original indexed minute query.
const DAY_MS = 86_400_000;
const DAY_BATCH = 8;

export async function readHistoryDays(db, venue, market, symbol, from, to, limit) {
  let cursor = Math.floor(from / DAY_MS) * DAY_MS;
  const lastDay = Math.floor(to / DAY_MS) * DAY_MS;
  const rows = [];
  while (cursor <= lastDay && rows.length <= limit) {
    const batchSize = Math.min(DAY_BATCH, Math.ceil((limit + 1 - rows.length) / 1440));
    const result = await db.prepare(`
      SELECT day_ms, samples FROM oi_history_days
      WHERE venue = ? AND market = ? AND symbol = ?
        AND day_ms >= ? AND day_ms <= ?
      ORDER BY day_ms ASC LIMIT ?
    `).bind(venue, market, symbol, cursor, lastDay, batchSize).all();
    const days = result.results ?? [];
    for (const day of days) {
      const samples = JSON.parse(day.samples);
      const buckets = Object.keys(samples).map(Number).sort((a, b) => a - b);
      for (const bucket of buckets) {
        if (bucket < from || bucket > to) continue;
        const [observed, usd, definition, raw, unit, mark] = samples[bucket];
        rows.push({ bucket_ms: bucket, observed_at_ms: observed,
          usd_open_interest: Number(usd), oi_definition: definition,
          raw_open_interest: Number(raw), raw_unit: unit, mark_price: Number(mark) });
        if (rows.length > limit) return rows;
      }
    }
    if (days.length < batchSize) break;
    cursor = Number(days[days.length - 1].day_ms) + DAY_MS;
  }
  return rows;
}
