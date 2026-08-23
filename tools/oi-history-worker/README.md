# Flowsurface OI history worker

Cloudflare Worker + D1 reference collector for the contract in
`docs/open-interest-history.md`. Its cron collects Bybit and Hyperliquid BTC
linear-perpetual OI once per minute. A VPS posts Binance raw OI and mark price
through the source-locked ingest route. D1 serves paged USD-normalized history.

## Deploy

1. `npx wrangler d1 create flowsurface-oi-history`
2. Put the returned database ID in `wrangler.jsonc`.
3. `npx wrangler d1 migrations apply flowsurface-oi-history --remote`
4. `npx wrangler secret put READ_TOKEN`
5. `npx wrangler secret put ADMIN_TOKEN`
6. `npx wrangler secret put INGEST_TOKEN`
7. `npx wrangler deploy`

Set the deployed URL and `READ_TOKEN` in Flowsurface Network settings. Use
`POST /admin/collect` with the admin bearer token for an immediate collection;
otherwise the `* * * * *` cron starts direct sources within one minute. The
VPS uses `POST /v1/ingest/open-interest` with its separate ingest token.
All secrets fail closed when absent; only `/health` is public.

Run `npm test` for the source-normalization and HTTP contract tests. No secret
or Cloudflare account is needed for those tests.
