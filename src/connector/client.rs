use exchange::adapter::{AdapterError, FetchError, MarketKind};
use exchange::unit::price::Price;
use exchange::unit::qty::{QtyNormalization, RawQtyUnit, SizeUnit, volume_size_unit};
use exchange::{OpenInterest, Ticker, TickerInfo, Trade, UnixMs};

use arrow_array::{Array, BooleanArray, Float64Array, Int64Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use exchange::adapter::{AdapterHandles, Venue};
use exchange::proxy::Proxy;

#[derive(Clone)]
pub struct DataSources {
    pub exchange: AdapterHandles,
    pub server: Option<ServerClient>,
    pub oi_history: Option<OiHistoryClient>,
}

impl DataSources {
    /// Build HTTP clients, spawn exchange adapter handles, and optionally
    /// create a market-data server client from the persisted configuration.
    pub fn new(network: &data::Network) -> Self {
        let proxy = network.proxy.as_ref();
        let (exchange_http, server_http) = Self::build_http_clients(proxy);

        let server = ServerClient::from_mode(&server_http, network);
        let oi_history = OiHistoryClient::from_network(&exchange_http, network);
        let exchange = AdapterHandles::spawn_venues(&exchange_http, Venue::ALL, proxy);

        Self {
            exchange,
            server,
            oi_history,
        }
    }

    /// Build the two HTTP clients used by the application.
    ///
    /// Returns `(exchange_client, server_client)`.
    ///
    /// * `exchange_client`: For exchange adapters (proxy-aware, no TLS
    ///   relaxation).
    /// * `server_client`: For the user-configured market-data server
    ///   (proxy-aware, accepts invalid TLS certificates for self-signed
    ///   server certs).
    ///
    /// # Panics
    ///
    /// Panics if either `reqwest::Client::builder().build()` fails.
    fn build_http_clients(proxy_cfg: Option<&Proxy>) -> (reqwest::Client, reqwest::Client) {
        let mut exchange_builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10));
        exchange_builder = exchange::adapter::proxy::try_apply_proxy(exchange_builder, proxy_cfg);
        let exchange_client = exchange_builder.build().unwrap_or_else(|e| {
            log::error!(
                "Fatal: failed to build exchange HTTP client - TLS backend missing, \
                 resource exhaustion, or invalid proxy config?\n  {e}"
            );
            panic!("Failed to build exchange HTTP client: {e}");
        });

        let mut server_builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .danger_accept_invalid_certs(true);
        server_builder = exchange::adapter::proxy::try_apply_proxy(server_builder, proxy_cfg);
        let server_client = server_builder.build().unwrap_or_else(|e| {
            log::error!(
                "Fatal: failed to build server HTTP client - TLS backend missing, \
                 resource exhaustion, or invalid proxy config?\n  {e}"
            );
            panic!("Failed to build server HTTP client: {e}");
        });

        (exchange_client, server_client)
    }
}

impl std::fmt::Debug for DataSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataSources")
            .field("exchange", &"…")
            .field("server", &self.server.as_ref().map(|_| "…"))
            .field("oi_history", &self.oi_history.as_ref().map(|_| "…"))
            .finish()
    }
}

const OI_HISTORY_VERSION: u8 = 1;
const OI_HISTORY_INTERVAL_MS: u64 = 60_000;
const OI_HISTORY_PAGE_SIZE: usize = 20_000;
const MAX_OI_HISTORY_PAGES: usize = 64;
const OI_HISTORY_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_OI_HISTORY_CACHE_ENTRIES: usize = 8;
const MAX_CACHED_OI_POINTS: usize = 100_000;

#[derive(Debug, Clone)]
pub struct OiHistoryClient {
    base_url: String,
    client: reqwest::Client,
    auth_token: Option<String>,
    cache: Arc<Mutex<OiHistoryCache>>,
}

#[derive(Debug, Default)]
struct OiHistoryCache {
    entries: FxHashMap<(Ticker, u64, u64), CachedOiHistory>,
}

#[derive(Debug, Clone)]
struct CachedOiHistory {
    fetched_at: Instant,
    values: Vec<OpenInterest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OiSourceKey {
    venue: String,
    market: String,
    symbol: String,
}

#[derive(Debug, Deserialize)]
struct OiHistoryResponse {
    version: u8,
    venue: String,
    market: String,
    symbol: String,
    interval_ms: u64,
    points: Vec<(u64, u64, f64)>,
    next_from: Option<u64>,
}

impl OiHistoryClient {
    fn new(base_url: &str, auth_token: Option<String>, client: reqwest::Client) -> Option<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let parsed = trimmed
            .parse::<url::Url>()
            .map_err(|err| log::warn!("Invalid OI history URL '{trimmed}': {err}"))
            .ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            log::warn!("OI history URL must use HTTP or HTTPS: {trimmed}");
            return None;
        }

        Some(Self {
            base_url: trimmed.to_string(),
            client,
            auth_token,
            cache: Arc::new(Mutex::new(OiHistoryCache::default())),
        })
    }

    fn from_network(http_client: &reqwest::Client, network: &data::Network) -> Option<Self> {
        let url = network.oi_history_url.as_deref()?;
        Self::new(
            url,
            network.oi_history_auth_token.clone(),
            http_client.clone(),
        )
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn fetch_open_interest(
        &self,
        ticker_info: TickerInfo,
        from: UnixMs,
        to: UnixMs,
    ) -> Result<Vec<OpenInterest>, AdapterError> {
        let Some(key) = oi_source_key(ticker_info) else {
            return Ok(Vec::new());
        };
        if from > to {
            return Ok(Vec::new());
        }
        let cache_key = (ticker_info.ticker, from.as_u64(), to.as_u64());
        if let Some(values) = self.cached_open_interest(cache_key) {
            return Ok(values);
        }

        let endpoint = format!("{}/v1/open-interest", self.base_url);
        let mut cursor = from.as_u64();
        let mut last_seen = None;
        let mut values = Vec::new();

        for page_number in 0..MAX_OI_HISTORY_PAGES {
            let mut request = self.client.get(&endpoint).query(&[
                ("venue", key.venue.as_str()),
                ("market", key.market.as_str()),
                ("symbol", key.symbol.as_str()),
            ]);
            request = request.query(&[
                ("from", cursor.to_string()),
                ("to", to.as_u64().to_string()),
                ("limit", OI_HISTORY_PAGE_SIZE.to_string()),
            ]);
            if let Some(token) = self.auth_token.as_deref() {
                request = request.bearer_auth(token);
            }

            let response = request.send().await.map_err(|err| {
                AdapterError::FetchError(FetchError::new(
                    format!("OI history request failed: {err}"),
                    "Open-interest history service unavailable. Check logs for details.",
                ))
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(AdapterError::http_status_failed(
                    status,
                    format!("OI history: {body}"),
                ));
            }
            let payload = response
                .json::<OiHistoryResponse>()
                .await
                .map_err(|err| AdapterError::ParseError(format!("OI history response: {err}")))?;
            let next = validate_oi_history_page(
                &payload,
                &key,
                cursor,
                to.as_u64(),
                &mut last_seen,
                &mut values,
            )?;
            let Some(next) = next else {
                self.cache_open_interest(cache_key, &values);
                return Ok(values);
            };
            cursor = next;

            if page_number + 1 == MAX_OI_HISTORY_PAGES {
                return Err(AdapterError::ParseError(
                    "OI history response exceeded the bounded page limit".to_string(),
                ));
            }
        }

        Ok(values)
    }

    fn cached_open_interest(&self, key: (Ticker, u64, u64)) -> Option<Vec<OpenInterest>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache
            .entries
            .retain(|_, entry| entry.fetched_at.elapsed() < OI_HISTORY_CACHE_TTL);
        cache.entries.get(&key).map(|entry| entry.values.clone())
    }

    fn cache_open_interest(&self, key: (Ticker, u64, u64), values: &[OpenInterest]) {
        if values.len() > MAX_CACHED_OI_POINTS {
            return;
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache
            .entries
            .retain(|_, entry| entry.fetched_at.elapsed() < OI_HISTORY_CACHE_TTL);
        if cache.entries.len() >= MAX_OI_HISTORY_CACHE_ENTRIES
            && !cache.entries.contains_key(&key)
            && let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.fetched_at)
                .map(|(key, _)| *key)
        {
            cache.entries.remove(&oldest);
        }
        cache.entries.insert(
            key,
            CachedOiHistory {
                fetched_at: Instant::now(),
                values: values.to_vec(),
            },
        );
    }
}

fn oi_source_key(ticker_info: TickerInfo) -> Option<OiSourceKey> {
    let venue = match ticker_info.exchange().venue() {
        Venue::Binance => "binance",
        Venue::Bybit => "bybit",
        Venue::Hyperliquid => "hyperliquid",
        Venue::Okex | Venue::Mexc => return None,
    };
    let market = match ticker_info.exchange().market_type() {
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
        MarketKind::Spot => return None,
    };
    Some(OiSourceKey {
        venue: venue.to_string(),
        market: market.to_string(),
        symbol: ticker_info
            .ticker
            .to_full_symbol_and_type()
            .0
            .to_ascii_uppercase(),
    })
}

fn validate_oi_history_page(
    payload: &OiHistoryResponse,
    expected: &OiSourceKey,
    cursor: u64,
    to: u64,
    last_seen: &mut Option<u64>,
    output: &mut Vec<OpenInterest>,
) -> Result<Option<u64>, AdapterError> {
    if payload.version != OI_HISTORY_VERSION
        || payload.interval_ms != OI_HISTORY_INTERVAL_MS
        || !payload.venue.eq_ignore_ascii_case(&expected.venue)
        || !payload.market.eq_ignore_ascii_case(&expected.market)
        || !payload.symbol.eq_ignore_ascii_case(&expected.symbol)
    {
        return Err(AdapterError::ParseError(
            "OI history response metadata did not match the request".to_string(),
        ));
    }

    for (bucket, observed_at, value) in &payload.points {
        if *bucket < cursor
            || *bucket > to
            || *bucket % OI_HISTORY_INTERVAL_MS != 0
            || *observed_at < *bucket
            || *observed_at >= bucket.saturating_add(OI_HISTORY_INTERVAL_MS)
            || last_seen.is_some_and(|last| *bucket <= last)
            || !value.is_finite()
            || *value < 0.0
        {
            return Err(AdapterError::ParseError(
                "OI history response contained invalid or unordered data".to_string(),
            ));
        }
        *last_seen = Some(*bucket);
        output.push(OpenInterest::snapshot(UnixMs::new(*observed_at), *value));
    }

    match payload.next_from {
        Some(next)
            if !payload.points.is_empty()
                && next > cursor
                && next <= to
                && last_seen.is_some_and(|last| next > last) =>
        {
            Ok(Some(next))
        }
        Some(_) => Err(AdapterError::ParseError(
            "OI history paging cursor did not advance".to_string(),
        )),
        None => Ok(None),
    }
}

/// A handle to a remote market-data HTTP server.
///
/// The server is expected to expose a `GET /trades.arrow` endpoint that
/// returns Arrow IPC streams of trade data.  Query parameters:
/// `venue`, `market`, `symbol`, `from`, `limit`.
#[derive(Debug, Clone)]
pub struct ServerClient {
    base_url: String,
    client: reqwest::Client,
    auth_token: Option<String>,
    coverage: Arc<RwLock<ServerCoverage>>,
}

#[derive(Debug, Default)]
struct ServerCoverage {
    fetched_at: Option<Instant>,
    earliest_by_ticker: FxHashMap<String, Option<UnixMs>>,
    ranges: FxHashMap<(String, u64, u64), CachedTradeCoverage>,
}

#[derive(Debug, Clone)]
struct CachedTradeCoverage {
    fetched_at: Instant,
    coverage: ServerTradeCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct ServerCoverageSegment {
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerTradeCoverage {
    pub segments: Vec<ServerCoverageSegment>,
}

impl ServerTradeCoverage {
    pub(crate) fn missing_ranges(&self, from: UnixMs, to: UnixMs) -> Vec<(UnixMs, UnixMs)> {
        coverage_missing_ranges(&self.segments, from.as_u64(), to.as_u64())
            .into_iter()
            .map(|(start, end)| (UnixMs::new(start), UnixMs::new(end)))
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct ServerCoverageResponse {
    version: u32,
    venue: String,
    market: String,
    symbol: String,
    requested_from: u64,
    requested_to: u64,
    proof: String,
    segments: Vec<ServerCoverageSegment>,
    complete: bool,
}

#[derive(Deserialize)]
struct ServerPairsResponse {
    pairs: Vec<ServerPair>,
}

#[derive(Deserialize)]
struct ServerPair {
    ticker: String,
    earliest: Option<u64>,
}

const SERVER_COVERAGE_TTL: Duration = Duration::from_secs(30);
const SERVER_COVERAGE_VERSION: u32 = 1;
const SERVER_COVERAGE_PROOF: &str = "recorder_capture_intervals_v1";
const MAX_SERVER_COVERAGE_CACHE_ENTRIES: usize = 256;

impl ServerClient {
    /// Create a new client targeting the given base URL
    /// (e.g. `http://127.0.0.1:8080`).
    ///
    /// The `client` is a shared `reqwest::Client` that carries the
    /// application-wide proxy and TLS configuration — it is **not** owned
    /// by this struct.
    ///
    /// An optional bearer token is sent as
    /// `Authorization: Bearer <token>` on every request.
    /// Trailing slashes are stripped. Returns `None` if the URL is empty
    /// or invalid.
    fn new(base_url: &str, auth_token: Option<String>, client: reqwest::Client) -> Option<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');

        if trimmed.is_empty() {
            return None;
        }

        let _ = trimmed
            .parse::<url::Url>()
            .map_err(|e| log::warn!("Invalid server base URL '{trimmed}': {e}"))
            .ok()?;

        Some(Self {
            base_url: trimmed.to_string(),
            client,
            auth_token,
            coverage: Arc::new(RwLock::new(ServerCoverage::default())),
        })
    }

    /// Base URL of the market-data server.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The shared `reqwest::Client` carrying proxy & TLS settings.
    fn http_client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Optional bearer token sent as `Authorization: Bearer <token>`.
    fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    /// Earliest currently stored trade for this server ticker.
    ///
    /// The short TTL preserves newly imported history while preventing every
    /// per-day backfill request from re-querying `/pairs`.
    pub(crate) async fn earliest_trade_time(
        &self,
        ticker_info: TickerInfo,
    ) -> Result<Option<UnixMs>, AdapterError> {
        let Some(ticker_key) = server_ticker_key(ticker_info) else {
            return Ok(None);
        };

        if let Ok(coverage) = self.coverage.read()
            && coverage
                .fetched_at
                .is_some_and(|fetched| fetched.elapsed() < SERVER_COVERAGE_TTL)
        {
            return Ok(coverage
                .earliest_by_ticker
                .get(&ticker_key)
                .copied()
                .flatten());
        }

        let url = format!("{}/pairs", self.base_url());
        let mut request = self.http_client().get(&url);
        if let Some(token) = self.auth_token() {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = request.send().await.map_err(|e| {
            AdapterError::FetchError(FetchError::new(
                format!("server coverage request failed: {e}"),
                "External data source error. Check logs for details.",
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AdapterError::http_status_failed(
                status,
                format!("server coverage: {body}"),
            ));
        }
        let payload = response
            .json::<ServerPairsResponse>()
            .await
            .map_err(|e| AdapterError::ParseError(format!("server coverage response: {e}")))?;
        let earliest_by_ticker = payload
            .pairs
            .into_iter()
            .map(|pair| {
                (
                    pair.ticker.to_ascii_lowercase(),
                    pair.earliest.map(UnixMs::new),
                )
            })
            .collect::<FxHashMap<_, _>>();
        let earliest = earliest_by_ticker.get(&ticker_key).copied().flatten();
        if let Ok(mut coverage) = self.coverage.write() {
            coverage.fetched_at = Some(Instant::now());
            coverage.earliest_by_ticker = earliest_by_ticker;
        }
        Ok(earliest)
    }

    /// Return the server's proven stored intervals for an exact trade range.
    ///
    /// The response is treated as untrusted metadata: identity, request bounds,
    /// schema version and segment ordering are all checked before any Arrow rows
    /// are accepted as coverage.
    pub(crate) async fn trade_coverage(
        &self,
        ticker_info: TickerInfo,
        from: UnixMs,
        to: UnixMs,
    ) -> Result<ServerTradeCoverage, AdapterError> {
        self.trade_coverage_inner(ticker_info, from, to, true).await
    }

    /// Refresh coverage without accepting the short-lived client cache.
    ///
    /// A just-closed trade range can race the recorder's two-second durable
    /// flush. The caller uses this only after a bounded delay for a tiny recent
    /// trailing gap; all historical queries continue to use the normal cache.
    pub(crate) async fn refresh_trade_coverage(
        &self,
        ticker_info: TickerInfo,
        from: UnixMs,
        to: UnixMs,
    ) -> Result<ServerTradeCoverage, AdapterError> {
        self.trade_coverage_inner(ticker_info, from, to, false)
            .await
    }

    async fn trade_coverage_inner(
        &self,
        ticker_info: TickerInfo,
        from: UnixMs,
        to: UnixMs,
        allow_cache: bool,
    ) -> Result<ServerTradeCoverage, AdapterError> {
        let (venue, market, symbol) = server_source_parts(ticker_info);
        let cache_key = (
            format!("{venue}:{market}:{symbol}"),
            from.as_u64(),
            to.as_u64(),
        );
        if allow_cache
            && let Ok(coverage) = self.coverage.read()
            && let Some(cached) = coverage.ranges.get(&cache_key)
            && cached.fetched_at.elapsed() < SERVER_COVERAGE_TTL
        {
            return Ok(cached.coverage.clone());
        }

        let url = format!("{}/coverage", self.base_url());
        let mut request = self
            .http_client()
            .get(&url)
            .query(&[
                ("venue", venue.as_str()),
                ("market", market.as_str()),
                ("symbol", symbol.as_str()),
            ])
            .query(&[("from", from.as_u64().to_string())])
            .query(&[("to", to.as_u64().to_string())]);
        if let Some(token) = self.auth_token() {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = request.send().await.map_err(|e| {
            AdapterError::FetchError(FetchError::new(
                format!("server coverage request failed: {e}"),
                "External data source error. Check logs for details.",
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AdapterError::http_status_failed(
                status,
                format!("server coverage: {body}"),
            ));
        }
        let payload = response
            .json::<ServerCoverageResponse>()
            .await
            .map_err(|e| AdapterError::ParseError(format!("server coverage response: {e}")))?;
        let coverage = validate_server_trade_coverage(
            payload,
            &venue,
            &market,
            &symbol,
            from.as_u64(),
            to.as_u64(),
        )?;

        if let Ok(mut cache) = self.coverage.write() {
            if cache.ranges.len() >= MAX_SERVER_COVERAGE_CACHE_ENTRIES {
                cache.ranges.clear();
            }
            cache.ranges.insert(
                cache_key,
                CachedTradeCoverage {
                    fetched_at: Instant::now(),
                    coverage: coverage.clone(),
                },
            );
        }
        Ok(coverage)
    }

    /// Build a [`ServerClient`] from the shared HTTP client and the current
    /// network configuration.
    ///
    /// Returns `None` when the mode is not [`TradeFetchMode::Server`] or the
    /// URL is empty/invalid.
    fn from_mode(http_client: &reqwest::Client, network: &data::Network) -> Option<Self> {
        match &network.trade_fetch_mode {
            data::TradeFetchMode::Server => {
                let url = network.server_url.as_deref()?;
                Self::new(url, network.server_auth_token.clone(), http_client.clone())
            }
            _ => None,
        }
    }

    /// Fetch trades from the server's **Arrow IPC** endpoint (`/trades.arrow`).
    pub async fn fetch_trades_arrow(
        &self,
        ticker_info: TickerInfo,
        from: UnixMs,
        to: UnixMs,
        limit: usize,
    ) -> Result<ParsedArrowBatch, AdapterError> {
        let (venue, market, symbol) = server_source_parts(ticker_info);

        let url = format!("{}/trades.arrow", self.base_url());

        log::debug!(
            "Querying server (arrow): {url} | venue={venue} market={market} symbol={symbol} from={from} to={to} limit={limit}",
        );

        let mut request = self
            .http_client()
            .get(&url)
            .query(&[
                ("venue", venue.as_str()),
                ("market", market.as_str()),
                ("symbol", symbol.as_str()),
            ])
            .query(&[("from", from.as_u64().to_string())])
            .query(&[("to", to.as_u64().to_string())])
            .query(&[("limit", limit.to_string())]);

        if let Some(token) = self.auth_token() {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }

        let response = request.send().await.map_err(|e| {
            log::warn!("Server arrow request failed: {e}");
            AdapterError::FetchError(FetchError::new(
                "server arrow request failed".to_string(),
                "External data source error. Check logs for details.",
            ))
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            log::warn!("Server returned {status}: {body}");
            return Err(AdapterError::http_status_failed(
                status,
                format!("server (arrow): {body}"),
            ));
        }

        if let Some(content_type) = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            && content_type != "application/vnd.apache.arrow.stream"
        {
            log::warn!(
                "Server returned unexpected Content-Type '{content_type}', \
                     expected 'application/vnd.apache.arrow.stream'"
            );
        }

        let bytes = response.bytes().await.map_err(|e| {
            log::warn!("Failed to read arrow response body: {e}");
            AdapterError::ParseError(format!("server (arrow): {e}"))
        })?;

        let parsed = parse_arrow_trades(bytes, &ticker_info)?;
        validate_server_trade_page(&parsed, from, to, limit)?;
        Ok(parsed)
    }
}

fn server_ticker_key(ticker_info: TickerInfo) -> Option<String> {
    let serialized = serde_json::to_value(ticker_info.ticker).ok()?;
    let (exchange, _) = serialized.as_str()?.split_once(':')?;
    let symbol = server_symbol(ticker_info);
    Some(format!("{exchange}:{}", symbol.to_ascii_lowercase()).to_ascii_lowercase())
}

fn server_source_parts(ticker_info: TickerInfo) -> (String, String, String) {
    let exchange = ticker_info.exchange();
    let venue = exchange.venue().to_string().to_ascii_lowercase();
    let market = exchange.market_type().to_string().to_ascii_lowercase();
    let symbol = server_symbol(ticker_info).to_ascii_lowercase();
    (venue, market, symbol)
}

/// Canonical symbol used by the recorder API.
///
/// Hyperliquid's public stream identifies native perpetuals by base asset
/// (`BTC`), while the recorder catalog identifies the same venue market by
/// its quoted display symbol (`BTCUSDC`). Fresh metadata normally carries that
/// display alias, but old saved layouts and the native metadata response can
/// legitimately restore an alias-less ticker. Falling back to raw `BTC` then
/// produces a truthful-but-empty coverage response for the wrong recorder key.
fn server_symbol(ticker_info: TickerInfo) -> String {
    if let Some(display) = ticker_info.ticker.display_symbol() {
        return display.to_owned();
    }

    let (native, market) = ticker_info.ticker.to_full_symbol_and_type();
    if ticker_info.exchange().venue() == Venue::Hyperliquid
        && market == MarketKind::LinearPerps
        // Builder-deployed markets carry a `dex:asset` identity and may use a
        // non-USDC collateral. Never guess their quote when metadata omitted
        // the display alias.
        && !native.contains(':')
        && !native.to_ascii_uppercase().ends_with("USDC")
    {
        format!("{native}USDC")
    } else {
        native
    }
}

fn validate_server_trade_coverage(
    payload: ServerCoverageResponse,
    expected_venue: &str,
    expected_market: &str,
    expected_symbol: &str,
    expected_from: u64,
    expected_to: u64,
) -> Result<ServerTradeCoverage, AdapterError> {
    if payload.version != SERVER_COVERAGE_VERSION
        || !payload.venue.eq_ignore_ascii_case(expected_venue)
        || !payload.market.eq_ignore_ascii_case(expected_market)
        || !payload.symbol.eq_ignore_ascii_case(expected_symbol)
        || payload.requested_from != expected_from
        || payload.requested_to != expected_to
        || payload.proof != SERVER_COVERAGE_PROOF
    {
        return Err(AdapterError::ParseError(
            "server coverage metadata did not match the requested source/range".into(),
        ));
    }

    let mut previous_to = None;
    for segment in &payload.segments {
        if segment.from > segment.to
            || segment.from < expected_from
            || segment.to > expected_to
            || previous_to.is_some_and(|end| segment.from <= end)
        {
            return Err(AdapterError::ParseError(
                "server coverage segments were invalid, overlapping, or unordered".into(),
            ));
        }
        previous_to = Some(segment.to);
    }

    let computed_complete =
        coverage_missing_ranges(&payload.segments, expected_from, expected_to).is_empty();
    if payload.complete != computed_complete {
        return Err(AdapterError::ParseError(
            "server coverage completion flag contradicted its segments".into(),
        ));
    }
    Ok(ServerTradeCoverage {
        segments: payload.segments,
    })
}

fn coverage_missing_ranges(
    segments: &[ServerCoverageSegment],
    from: u64,
    to: u64,
) -> Vec<(u64, u64)> {
    if from > to {
        return Vec::new();
    }
    let mut missing = Vec::new();
    let mut cursor = from;
    for segment in segments {
        if segment.from > cursor {
            missing.push((cursor, segment.from.saturating_sub(1)));
        }
        cursor = cursor.max(segment.to.saturating_add(1));
        if cursor > to {
            break;
        }
    }
    if cursor <= to {
        missing.push((cursor, to));
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, adapter::Exchange};

    #[test]
    fn server_coverage_key_uses_hyperliquid_display_symbol() {
        let source = TickerInfo::new(
            Ticker::new_with_display("BTC", Exchange::HyperliquidLinear, Some("BTCUSDC")),
            1.0,
            0.001,
            None,
        );

        assert_eq!(
            server_ticker_key(source).as_deref(),
            Some("hyperliquidlinear:btcusdc")
        );
    }

    #[test]
    fn server_coverage_key_restores_aliasless_native_hyperliquid_perp() {
        let source = TickerInfo::new(
            Ticker::new("BTC", Exchange::HyperliquidLinear),
            1.0,
            0.001,
            None,
        );

        assert_eq!(
            server_ticker_key(source).as_deref(),
            Some("hyperliquidlinear:btcusdc")
        );
        assert_eq!(
            server_source_parts(source),
            (
                "hyperliquid".to_string(),
                "linear".to_string(),
                "btcusdc".to_string(),
            )
        );
    }

    #[test]
    fn server_symbol_does_not_guess_builder_market_collateral() {
        let source = TickerInfo::new(
            Ticker::new("hyna:BTC", Exchange::HyperliquidLinear),
            1.0,
            0.001,
            None,
        );

        assert_eq!(server_symbol(source), "hyna:BTC");
    }

    #[test]
    fn server_coverage_key_uses_native_symbol_without_alias() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );

        assert_eq!(
            server_ticker_key(source).as_deref(),
            Some("binancelinear:btcusdt")
        );
    }

    #[test]
    fn server_coverage_complement_preserves_internal_and_boundary_gaps() {
        let segments = [
            ServerCoverageSegment { from: 110, to: 119 },
            ServerCoverageSegment { from: 130, to: 140 },
        ];
        assert_eq!(
            coverage_missing_ranges(&segments, 100, 150),
            vec![(100, 109), (120, 129), (141, 150)]
        );
    }

    #[test]
    fn server_coverage_rejects_false_completion() {
        let payload = ServerCoverageResponse {
            version: SERVER_COVERAGE_VERSION,
            venue: "bybit".into(),
            market: "linear".into(),
            symbol: "btcusdt".into(),
            requested_from: 100,
            requested_to: 200,
            proof: SERVER_COVERAGE_PROOF.into(),
            segments: vec![ServerCoverageSegment { from: 100, to: 150 }],
            complete: true,
        };
        assert!(
            validate_server_trade_coverage(payload, "bybit", "linear", "btcusdt", 100, 200)
                .is_err()
        );
    }

    fn valid_trade(time: u64) -> Trade {
        Trade {
            time: UnixMs::new(time),
            is_sell: false,
            price: Price::from_f64(100.0),
            qty: exchange::unit::qty::Qty::from_f64(1.0),
        }
    }

    #[test]
    fn server_trade_page_accepts_only_complete_ordered_bounded_rows() {
        let parsed = ParsedArrowBatch {
            trades: vec![valid_trade(100), valid_trade(101)],
            raw_row_count: 2,
            last_ts: Some(UnixMs::new(101)),
        };

        assert!(validate_server_trade_page(&parsed, UnixMs::new(100), UnixMs::new(101), 2).is_ok());
    }

    #[test]
    fn server_trade_page_rejects_missing_or_unordered_rows() {
        let missing = ParsedArrowBatch {
            trades: vec![valid_trade(100)],
            raw_row_count: 2,
            last_ts: Some(UnixMs::new(101)),
        };
        assert!(
            validate_server_trade_page(&missing, UnixMs::new(100), UnixMs::new(101), 2).is_err()
        );

        let unordered = ParsedArrowBatch {
            trades: vec![valid_trade(101), valid_trade(100)],
            raw_row_count: 2,
            last_ts: Some(UnixMs::new(100)),
        };
        assert!(
            validate_server_trade_page(&unordered, UnixMs::new(100), UnixMs::new(101), 2).is_err()
        );
    }

    #[test]
    fn oi_history_source_uses_native_venue_symbol() {
        let source = TickerInfo::new(
            Ticker::new_with_display("BTC", Exchange::HyperliquidLinear, Some("BTCUSDC")),
            1.0,
            0.001,
            None,
        );

        assert_eq!(
            oi_source_key(source),
            Some(OiSourceKey {
                venue: "hyperliquid".to_string(),
                market: "linear".to_string(),
                symbol: "BTC".to_string(),
            })
        );
    }

    #[test]
    fn oi_history_page_rejects_false_or_unordered_data() {
        let expected = OiSourceKey {
            venue: "binance".to_string(),
            market: "linear".to_string(),
            symbol: "BTCUSDT".to_string(),
        };
        let mut output = Vec::new();
        let mut last_seen = None;
        let payload = OiHistoryResponse {
            version: OI_HISTORY_VERSION,
            venue: expected.venue.clone(),
            market: expected.market.clone(),
            symbol: expected.symbol.clone(),
            interval_ms: OI_HISTORY_INTERVAL_MS,
            points: vec![(120_000, 121_000, 10.0), (60_000, 61_000, 11.0)],
            next_from: None,
        };

        assert!(
            validate_oi_history_page(
                &payload,
                &expected,
                60_000,
                180_000,
                &mut last_seen,
                &mut output,
            )
            .is_err()
        );
    }

    #[test]
    fn oi_history_page_accepts_exact_minute_usd_values() {
        let expected = OiSourceKey {
            venue: "bybit".to_string(),
            market: "linear".to_string(),
            symbol: "BTCUSDT".to_string(),
        };
        let mut output = Vec::new();
        let mut last_seen = None;
        let payload = OiHistoryResponse {
            version: OI_HISTORY_VERSION,
            venue: expected.venue.clone(),
            market: expected.market.clone(),
            symbol: expected.symbol.clone(),
            interval_ms: OI_HISTORY_INTERVAL_MS,
            points: vec![(60_000, 61_000, 10.0), (120_000, 121_000, 11.0)],
            next_from: Some(180_000),
        };

        assert_eq!(
            validate_oi_history_page(
                &payload,
                &expected,
                60_000,
                240_000,
                &mut last_seen,
                &mut output,
            )
            .expect("valid page"),
            Some(180_000)
        );
        assert_eq!(output.len(), 2);
        assert_eq!(output[1].value, 11.0);
    }

    #[test]
    fn cloned_oi_clients_share_a_bounded_short_lived_cache() {
        let client = OiHistoryClient::new("https://oi.example", None, reqwest::Client::new())
            .expect("OI client");
        let clone = client.clone();
        let key = (
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            60_000,
            120_000,
        );
        let values = [OpenInterest::snapshot(UnixMs::new(61_000), 10.0)];

        client.cache_open_interest(key, &values);

        assert_eq!(clone.cached_open_interest(key), Some(values.to_vec()));
    }
}

/// Expected Arrow column names in the IPC stream.
const ARROW_TS_COL: &str = "ts";
const ARROW_PRICE_COL: &str = "price";
const ARROW_QTY_COL: &str = "qty";
const ARROW_IS_SELL_COL: &str = "is_sell";

/// Resolved column indices for the four required Arrow fields.
struct TradeColumns {
    ts: usize,
    price: usize,
    qty: usize,
    is_sell: usize,
}

/// Validate the Arrow stream schema up-front: resolve column indices by
/// name, verify the expected Arrow data types, and fail fast on any
/// mismatch before a single row is parsed.
fn validate_arrow_schema(schema: &arrow_schema::Schema) -> Result<TradeColumns, AdapterError> {
    use arrow_schema::DataType;

    let index_of = |name: &str| {
        schema.index_of(name).map_err(|_| {
            AdapterError::ParseError(format!(
                "Arrow stream schema missing required column '{name}'"
            ))
        })
    };

    let cols = TradeColumns {
        ts: index_of(ARROW_TS_COL)?,
        price: index_of(ARROW_PRICE_COL)?,
        qty: index_of(ARROW_QTY_COL)?,
        is_sell: index_of(ARROW_IS_SELL_COL)?,
    };

    let check_type = |idx: usize, name: &str, expected: &DataType| -> Result<(), AdapterError> {
        let field = schema.field(idx);
        if field.data_type() != expected {
            return Err(AdapterError::ParseError(format!(
                "Arrow stream schema column '{name}': expected {expected:?}, got {:?}",
                field.data_type()
            )));
        }
        Ok(())
    };

    check_type(cols.ts, ARROW_TS_COL, &DataType::Int64)?;
    check_type(cols.price, ARROW_PRICE_COL, &DataType::Float64)?;
    check_type(cols.qty, ARROW_QTY_COL, &DataType::Float64)?;
    check_type(cols.is_sell, ARROW_IS_SELL_COL, &DataType::Boolean)?;

    Ok(cols)
}

/// Result of parsing an Arrow IPC trade stream.
///
/// Separates displayable trades from stream-level metadata so the
/// paging loop can distinguish "server returned rows but all were
/// null-filtered" from "server genuinely had no data for this range."
#[derive(Debug)]
pub struct ParsedArrowBatch {
    /// Valid, non-null trades ready for display.
    pub trades: Vec<Trade>,
    /// Total rows received across all batches in the stream.
    pub raw_row_count: usize,
    /// Latest non-null `ts` seen across *all* rows (including rows
    /// filtered out due to nulls in other columns).  Used for cursor
    /// advancement in the paging loop.
    pub last_ts: Option<UnixMs>,
}

/// Fail closed on malformed recorder pages before any row reaches an
/// indicator or a durable cache. Coverage metadata proves that an interval was
/// recorded; every Arrow row inside that interval must still be complete,
/// ordered, bounded by the request, and economically meaningful.
fn validate_server_trade_page(
    parsed: &ParsedArrowBatch,
    from: UnixMs,
    to: UnixMs,
    limit: usize,
) -> Result<(), AdapterError> {
    if parsed.raw_row_count > limit {
        return Err(AdapterError::ParseError(
            "server trade page exceeded the requested row limit".into(),
        ));
    }
    if parsed.raw_row_count != parsed.trades.len() {
        return Err(AdapterError::ParseError(
            "server trade page contained null or otherwise unusable required fields".into(),
        ));
    }
    if parsed.trades.is_empty() {
        return if parsed.last_ts.is_none() {
            Ok(())
        } else {
            Err(AdapterError::ParseError(
                "empty server trade page reported a paging timestamp".into(),
            ))
        };
    }

    let first = parsed.trades.first().expect("non-empty checked above");
    let last = parsed.trades.last().expect("non-empty checked above");
    if first.time < from || last.time > to || parsed.last_ts != Some(last.time) {
        return Err(AdapterError::ParseError(
            "server trade page fell outside its requested range or reported a false cursor".into(),
        ));
    }
    if parsed
        .trades
        .windows(2)
        .any(|pair| pair[0].time > pair[1].time)
    {
        return Err(AdapterError::ParseError(
            "server trade page was not ordered by timestamp".into(),
        ));
    }
    if parsed
        .trades
        .iter()
        .any(|trade| trade.price.units <= 0 || trade.qty.units <= 0)
    {
        return Err(AdapterError::ParseError(
            "server trade page contained a non-positive price or quantity".into(),
        ));
    }
    Ok(())
}

/// Parse raw Arrow IPC stream bytes into a [`ParsedArrowBatch`].
///
/// Expects the Arrow IPC streaming format. The four required columns
/// (`ts`, `price`, `qty`, `is_sell`) are looked up by name. Column order
/// is not significant and extra columns are silently ignored.
fn parse_arrow_trades(
    data: bytes::Bytes,
    ticker_info: &TickerInfo,
) -> Result<ParsedArrowBatch, AdapterError> {
    let reader = StreamReader::try_new(std::io::Cursor::new(data), None)
        .map_err(|e| AdapterError::ParseError(format!("arrow stream open: {e}")))?;

    // Resolve column positions by name and validate types once up-front.
    let cols = validate_arrow_schema(&reader.schema())?;

    let market_kind = ticker_info.exchange().market_type();
    let raw_qty_unit = match market_kind {
        MarketKind::InversePerps => RawQtyUnit::Quote,
        MarketKind::Spot | MarketKind::LinearPerps => RawQtyUnit::Base,
    };

    let size_in_quote_ccy = volume_size_unit() == SizeUnit::Quote;
    let qty_norm =
        QtyNormalization::with_raw_qty_unit(size_in_quote_ccy, *ticker_info, raw_qty_unit);

    let mut trades = Vec::new();
    let mut raw_row_count: usize = 0;
    let mut last_ts: Option<UnixMs> = None;
    let mut skipped_total: u64 = 0;

    for result in reader {
        let batch: RecordBatch =
            result.map_err(|e| AdapterError::ParseError(format!("arrow batch: {e}")))?;

        raw_row_count += batch.num_rows();

        let ts_col = batch
            .column(cols.ts)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| {
                AdapterError::ParseError(format!("column '{ARROW_TS_COL}' is not Int64"))
            })?;
        let price_col = batch
            .column(cols.price)
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or_else(|| {
                AdapterError::ParseError(format!("column '{ARROW_PRICE_COL}' is not Float64"))
            })?;
        let qty_col = batch
            .column(cols.qty)
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or_else(|| {
                AdapterError::ParseError(format!("column '{ARROW_QTY_COL}' is not Float64"))
            })?;
        let is_sell_col = batch
            .column(cols.is_sell)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or_else(|| {
                AdapterError::ParseError(format!("column '{ARROW_IS_SELL_COL}' is not Boolean"))
            })?;

        let has_nulls = ts_col.null_count() > 0
            || price_col.null_count() > 0
            || qty_col.null_count() > 0
            || is_sell_col.null_count() > 0;

        if has_nulls {
            let mut skipped_in_batch: u64 = 0;

            for i in 0..batch.num_rows() {
                // Track the latest non-null ts across *all* rows for
                // cursor advancement, even if the row is filtered out.
                if !ts_col.is_null(i) {
                    let ts = ts_col.value(i).max(0) as u64;
                    let candidate = UnixMs::new(ts);
                    last_ts = Some(last_ts.map_or(candidate, |prev| prev.max(candidate)));
                }

                if ts_col.is_null(i)
                    || price_col.is_null(i)
                    || qty_col.is_null(i)
                    || is_sell_col.is_null(i)
                {
                    skipped_in_batch += 1;
                    continue;
                }
                let ts = ts_col.value(i).max(0) as u64;
                trades.push(Trade {
                    time: UnixMs::new(ts),
                    is_sell: is_sell_col.value(i),
                    price: Price::from_f64(price_col.value(i)),
                    qty: qty_norm.normalize_qty(qty_col.value(i), price_col.value(i)),
                });
            }

            skipped_total += skipped_in_batch;
        } else {
            for i in 0..batch.num_rows() {
                let ts = ts_col.value(i).max(0) as u64;
                trades.push(Trade {
                    time: UnixMs::new(ts),
                    is_sell: is_sell_col.value(i),
                    price: Price::from_f64(price_col.value(i)),
                    qty: qty_norm.normalize_qty(qty_col.value(i), price_col.value(i)),
                });
            }
        }
    }

    // Fast path: if no nulls were encountered, last_ts is simply the
    // timestamp of the last valid trade.
    if last_ts.is_none() {
        last_ts = trades.last().map(|t| t.time);
    }

    if skipped_total > 0 {
        log::warn!(
            "Arrow stream for {} ({}): skipped {skipped_total} of {raw_row_count} row(s) with nulls in required fields",
            ticker_info.ticker,
            ticker_info.exchange(),
        );
    }

    if trades.is_empty() {
        log::info!(
            "Arrow stream contained no valid trades for {} ({}) (raw rows: {raw_row_count})",
            ticker_info.ticker,
            ticker_info.exchange(),
        );
    }

    Ok(ParsedArrowBatch {
        trades,
        raw_row_count,
        last_ts,
    })
}
