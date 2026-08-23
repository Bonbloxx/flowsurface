const MINUTE_MS = 60_000;
const DEFAULT_PAGE_SIZE = 10_000;
const MAX_PAGE_SIZE = 20_000;
const MAX_INGEST_BODY_BYTES = 4_096;
const MAX_INGEST_AGE_MS = 10 * MINUTE_MS;
const MAX_INGEST_FUTURE_SKEW_MS = MINUTE_MS;

const BINANCE_SOURCE = Object.freeze({
  venue: "binance",
  market: "linear",
  symbol: "BTCUSDT",
});

// Binance blocks Cloudflare egress at its edge. The VPS collector owns that
// source; the Worker continues collecting venues that are reachable directly.
const DIRECT_SOURCES = Object.freeze([
  { venue: "bybit", market: "linear", symbol: "BTCUSDT", collect: collectBybit },
  { venue: "hyperliquid", market: "linear", symbol: "BTC", collect: collectHyperliquid },
]);

function finiteNonNegative(value, field) {
  const number = Number(value);
  if (!Number.isFinite(number) || number < 0) {
    throw new Error(`invalid ${field}: ${value}`);
  }
  return number;
}

function positive(value, field) {
  const number = finiteNonNegative(value, field);
  if (number === 0) throw new Error(`invalid ${field}: ${value}`);
  return number;
}

function sample(source, observedAtMs, rawOpenInterest, rawUnit, markPrice, usdOpenInterest) {
  const observed = finiteNonNegative(observedAtMs, "observation time");
  const raw = finiteNonNegative(rawOpenInterest, "raw open interest");
  const mark = positive(markPrice, "mark price");
  const usd = finiteNonNegative(usdOpenInterest, "USD open interest");
  return {
    ...source,
    bucketMs: Math.floor(observed / MINUTE_MS) * MINUTE_MS,
    observedAtMs: observed,
    rawOpenInterest: raw,
    rawUnit,
    markPrice: mark,
    usdOpenInterest: usd,
    oiDefinition: "venue_reported",
  };
}

async function json(fetcher, url, init) {
  const response = await fetcher(url, init);
  if (!response.ok) {
    throw new Error(`${url} returned ${response.status}`);
  }
  return response.json();
}

export function normalizeBinance(openInterest, premiumIndex) {
  const raw = finiteNonNegative(openInterest.openInterest, "Binance open interest");
  const mark = positive(premiumIndex.markPrice, "Binance mark price");
  return sample(BINANCE_SOURCE, openInterest.time, raw, "base", mark, raw * mark);
}

export function normalizeBybit(payload) {
  if (Number(payload.retCode) !== 0) {
    throw new Error(`Bybit returned ${payload.retCode}: ${payload.retMsg ?? "unknown error"}`);
  }
  const item = payload.result?.list?.[0];
  if (!item) throw new Error("Bybit ticker response is empty");
  const mark = positive(item.markPrice, "Bybit mark price");
  const raw = item.openInterest != null
    ? finiteNonNegative(item.openInterest, "Bybit both-side open interest")
    : finiteNonNegative(item.singleOpenInterest, "Bybit single-side open interest") * 2;
  const usd = item.openInterestValue != null
    ? finiteNonNegative(item.openInterestValue, "Bybit both-side OI value")
    : item.singleOpenInterestValue != null
      ? finiteNonNegative(item.singleOpenInterestValue, "Bybit single-side OI value") * 2
      : raw * mark;
  return sample(
    { venue: "bybit", market: "linear", symbol: "BTCUSDT" },
    payload.time,
    raw,
    "base",
    mark,
    usd,
  );
}

async function collectBybit(fetcher) {
  const payload = await json(
    fetcher,
    "https://api.bybit.com/v5/market/tickers?category=linear&symbol=BTCUSDT",
  );
  return normalizeBybit(payload);
}

export function normalizeHyperliquid(payload, observedAtMs) {
  const universe = payload?.[0]?.universe;
  const contexts = payload?.[1];
  if (!Array.isArray(universe) || !Array.isArray(contexts)) {
    throw new Error("Hyperliquid asset contexts response is invalid");
  }
  const index = universe.findIndex((asset) => asset?.name === "BTC");
  if (index < 0 || !contexts[index]) throw new Error("Hyperliquid BTC context is missing");
  const context = contexts[index];
  const raw = finiteNonNegative(context.openInterest, "Hyperliquid open interest");
  const mark = positive(context.markPx, "Hyperliquid mark price");
  return sample(
    { venue: "hyperliquid", market: "linear", symbol: "BTC" },
    observedAtMs,
    raw,
    "base",
    mark,
    raw * mark,
  );
}

async function collectHyperliquid(fetcher, observedAtMs) {
  const payload = await json(fetcher, "https://api.hyperliquid.xyz/info", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ type: "metaAndAssetCtxs" }),
  });
  return normalizeHyperliquid(payload, observedAtMs);
}

const UPSERT = `
INSERT INTO oi_samples (
  venue, market, symbol, bucket_ms, observed_at_ms,
  raw_open_interest, raw_unit, mark_price, usd_open_interest, oi_definition
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT (venue, market, symbol, bucket_ms) DO UPDATE SET
  observed_at_ms = excluded.observed_at_ms,
  raw_open_interest = excluded.raw_open_interest,
  raw_unit = excluded.raw_unit,
  mark_price = excluded.mark_price,
  usd_open_interest = excluded.usd_open_interest,
  oi_definition = excluded.oi_definition
WHERE excluded.observed_at_ms >= oi_samples.observed_at_ms`;

export async function collectAll(env, observedAtMs = Date.now(), fetcher = fetch) {
  const results = await Promise.allSettled(
    DIRECT_SOURCES.map((source) => source.collect(fetcher, observedAtMs)),
  );
  const samples = [];
  for (let index = 0; index < results.length; index += 1) {
    const result = results[index];
    if (result.status === "fulfilled") {
      samples.push(result.value);
    } else {
      console.error(JSON.stringify({
        event: "oi_collection_failed",
        venue: DIRECT_SOURCES[index].venue,
        error: String(result.reason?.message ?? result.reason),
      }));
    }
  }
  if (samples.length === 0) throw new Error("all OI sources failed");

  await persistSamples(env, samples);
  return samples;
}

async function persistSamples(env, samples) {
  const statements = samples.map((value) => env.DB.prepare(UPSERT).bind(
    value.venue,
    value.market,
    value.symbol,
    value.bucketMs,
    value.observedAtMs,
    value.rawOpenInterest,
    value.rawUnit,
    value.markPrice,
    value.usdOpenInterest,
    value.oiDefinition,
  ));
  await env.DB.batch(statements);
}

async function bearerAuthorized(request, token) {
  const actual = request.headers.get("authorization");
  if (typeof token !== "string" || token.length === 0 || actual == null) {
    return false;
  }
  const encoder = new TextEncoder();
  const [actualHash, expectedHash] = await Promise.all([
    crypto.subtle.digest("SHA-256", encoder.encode(actual)),
    crypto.subtle.digest("SHA-256", encoder.encode(`Bearer ${token}`)),
  ]);
  if (typeof crypto.subtle.timingSafeEqual === "function") {
    return crypto.subtle.timingSafeEqual(actualHash, expectedHash);
  }
  // Node's test WebCrypto does not yet expose the Workers extension. Both
  // inputs are fixed-size SHA-256 digests, so keep the local-test fallback on
  // a full fixed-length pass. Production uses timingSafeEqual above.
  const actualBytes = new Uint8Array(actualHash);
  const expectedBytes = new Uint8Array(expectedHash);
  let difference = 0;
  for (let index = 0; index < actualBytes.length; index += 1) {
    difference |= actualBytes[index] ^ expectedBytes[index];
  }
  return difference === 0;
}

function responseJson(value, status = 200, extraHeaders = {}) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", ...extraHeaders },
  });
}

function integerParameter(url, name, fallback) {
  const raw = url.searchParams.get(name);
  if (raw == null) return fallback;
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`invalid ${name}`);
  return value;
}

async function history(request, env) {
  if (!await bearerAuthorized(request, env.READ_TOKEN)) {
    return responseJson({ error: "unauthorized" }, 401);
  }
  const url = new URL(request.url);
  const venue = url.searchParams.get("venue")?.toLowerCase();
  const market = url.searchParams.get("market")?.toLowerCase();
  const symbol = url.searchParams.get("symbol")?.toUpperCase();
  if (!venue || !market || !symbol) {
    return responseJson({ error: "venue, market and symbol are required" }, 400);
  }

  try {
    const from = integerParameter(url, "from", 0);
    const to = integerParameter(url, "to", Date.now());
    const limit = Math.min(integerParameter(url, "limit", DEFAULT_PAGE_SIZE), MAX_PAGE_SIZE);
    if (from > to || limit === 0) throw new Error("invalid range");
    const query = env.DB.prepare(`
      SELECT bucket_ms, observed_at_ms, usd_open_interest, oi_definition
      FROM oi_samples
      WHERE venue = ? AND market = ? AND symbol = ?
        AND bucket_ms >= ? AND bucket_ms <= ?
      ORDER BY bucket_ms ASC
      LIMIT ?
    `).bind(venue, market, symbol, from, to, limit + 1);
    const result = await query.all();
    const rows = result.results ?? [];
    const page = rows.slice(0, limit);
    const nextFrom = rows.length > limit
      ? Number(page[page.length - 1].bucket_ms) + MINUTE_MS
      : null;
    return responseJson({
      version: 1,
      venue,
      market,
      symbol,
      interval_ms: MINUTE_MS,
      points: page.map((row) => [
        Number(row.bucket_ms),
        Number(row.observed_at_ms),
        row.oi_definition === "legacy" && venue === "bybit"
          ? Number(row.usd_open_interest) * 2
          : Number(row.usd_open_interest),
      ]),
      next_from: nextFrom,
    }, 200, { "cache-control": "private, max-age=30" });
  } catch (error) {
    return responseJson({ error: String(error.message ?? error) }, 400);
  }
}

async function boundedJson(request) {
  const declaredLength = request.headers.get("content-length");
  if (declaredLength != null) {
    const length = Number(declaredLength);
    if (!Number.isSafeInteger(length) || length < 0 || length > MAX_INGEST_BODY_BYTES) {
      throw new Error("request body is too large");
    }
  }
  const text = await request.text();
  if (new TextEncoder().encode(text).byteLength > MAX_INGEST_BODY_BYTES) {
    throw new Error("request body is too large");
  }
  return JSON.parse(text);
}

function normalizeIngestedBinance(payload, nowMs) {
  if (payload == null || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("request body must be an object");
  }
  if (payload.version !== 1
    || payload.venue !== BINANCE_SOURCE.venue
    || payload.market !== BINANCE_SOURCE.market
    || payload.symbol !== BINANCE_SOURCE.symbol) {
    throw new Error("unsupported source");
  }
  const observedAtMs = finiteNonNegative(payload.observed_at_ms, "observation time");
  if (!Number.isSafeInteger(observedAtMs)
    || observedAtMs < nowMs - MAX_INGEST_AGE_MS
    || observedAtMs > nowMs + MAX_INGEST_FUTURE_SKEW_MS) {
    throw new Error("observation time is outside the accepted live window");
  }
  const raw = finiteNonNegative(payload.raw_open_interest, "Binance open interest");
  const mark = positive(payload.mark_price, "Binance mark price");
  return sample(BINANCE_SOURCE, observedAtMs, raw, "base", mark, raw * mark);
}

async function ingestOpenInterest(request, env, nowMs) {
  if (!await bearerAuthorized(request, env.INGEST_TOKEN)) {
    return responseJson({ error: "unauthorized" }, 401);
  }
  try {
    const payload = await boundedJson(request);
    const value = normalizeIngestedBinance(payload, nowMs);
    await persistSamples(env, [value]);
    console.log(JSON.stringify({
      event: "oi_ingested",
      venue: value.venue,
      symbol: value.symbol,
      bucket_ms: value.bucketMs,
      observed_at_ms: value.observedAtMs,
    }));
    return responseJson({
      accepted: true,
      bucket_ms: value.bucketMs,
      observed_at_ms: value.observedAtMs,
    }, 201);
  } catch (error) {
    return responseJson({ error: String(error.message ?? error) }, 400);
  }
}

export async function route(request, env, fetcher = fetch, nowMs = Date.now()) {
  const url = new URL(request.url);
  if (request.method === "GET" && url.pathname === "/health") {
    return responseJson({ status: "ok", version: 1 });
  }
  if (request.method === "GET" && url.pathname === "/v1/open-interest") {
    return history(request, env);
  }
  if (request.method === "POST" && url.pathname === "/v1/ingest/open-interest") {
    return ingestOpenInterest(request, env, nowMs);
  }
  if (request.method === "POST" && url.pathname === "/admin/collect") {
    if (!await bearerAuthorized(request, env.ADMIN_TOKEN)) {
      return responseJson({ error: "unauthorized" }, 401);
    }
    try {
      const samples = await collectAll(env, Date.now(), fetcher);
      return responseJson({ collected: samples.length });
    } catch (error) {
      return responseJson({ error: String(error.message ?? error) }, 502);
    }
  }
  return responseJson({ error: "not found" }, 404);
}

export default {
  async scheduled(controller, env) {
    await collectAll(env, controller.scheduledTime);
  },
  async fetch(request, env) {
    return route(request, env);
  },
};
