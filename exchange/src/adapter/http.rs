use crate::adapter::AdapterError;
use crate::adapter::limiter::RateLimiter;
use crate::depth::DepthPayload;
use crate::{Kline, OpenInterest, Ticker, TickerInfo, TickerStats, Timeframe, Trade, UnixMs};

use futures::future::BoxFuture;
use reqwest::{Client, Method, Response, header};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

type ResponseTx<T> = oneshot::Sender<Result<T, AdapterError>>;

pub(super) type TickerMetadataMap = HashMap<Ticker, Option<TickerInfo>>;
pub(super) type TickerStatsMap = HashMap<Ticker, TickerStats>;

pub(super) enum FetchCommand<M> {
    TickerMetadata {
        market_scope: M,
        reply: ResponseTx<TickerMetadataMap>,
    },
    TickerStats {
        market_scope: M,
        reply: ResponseTx<TickerStatsMap>,
    },
    Klines {
        ticker: TickerInfo,
        timeframe: Timeframe,
        range: Option<(UnixMs, UnixMs)>,
        reply: ResponseTx<Vec<Kline>>,
    },
    OpenInterest {
        ticker: TickerInfo,
        timeframe: Timeframe,
        range: Option<(UnixMs, UnixMs)>,
        reply: ResponseTx<Vec<OpenInterest>>,
    },
    DepthSnapshot {
        ticker: Ticker,
        reply: ResponseTx<DepthPayload>,
    },
    Trades {
        ticker: TickerInfo,
        from_time: UnixMs,
        to_time: Option<UnixMs>,
        data_path: Option<PathBuf>,
        reply: ResponseTx<Vec<Trade>>,
    },
}

pub(super) struct HttpHub<L> {
    client: Client,
    /// Shared behind a mutex so several in-flight requests (for example
    /// parallel trade pages) can pace against one rate budget. Locks are
    /// only held across synchronous limiter updates, never across awaits.
    limiter: std::sync::Mutex<L>,
}

impl<L: RateLimiter> HttpHub<L> {
    pub(super) fn with_client(client: Client, limiter: L) -> Self {
        Self {
            client,
            limiter: std::sync::Mutex::new(limiter),
        }
    }

    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    fn lock_limiter(limiter: &std::sync::Mutex<L>) -> std::sync::MutexGuard<'_, L> {
        match limiter.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Lowest-level HTTP layer.
    ///
    /// Applies limiter pre-wait, performs the request, updates limiter state from
    /// the response, and returns the raw response for callers that need custom
    /// decoding/parsing logic.
    ///
    /// Rate-limit rejections (429/418) are retried with server-provided
    /// backoff instead of failing the whole fetch — bursts that slip past the
    /// local limiter must not surface as UI errors.
    pub(super) async fn http_response_with_limiter(
        &self,
        url: &str,
        weight: usize,
        method: Method,
        json_body: Option<&serde_json::Value>,
    ) -> Result<Response, AdapterError> {
        const MAX_RATE_LIMIT_RETRIES: u32 = 3;
        let request_method = method.clone();

        let mut attempt: u32 = 0;
        loop {
            let wait_time = {
                let mut limiter = Self::lock_limiter(&self.limiter);
                limiter.prepare_request(weight)
            };
            if let Some(wait_time) = wait_time {
                log::warn!("Rate limit hit for: {url}. Waiting for {:?}", wait_time);
                tokio::time::sleep(wait_time).await;
            }

            let response = Self::send_request_client(self.client(), method.clone(), url, json_body)
                .await
                .map_err(|error| AdapterError::request_failed(&request_method, url, error))?;

            let rate_limited = {
                let limiter = Self::lock_limiter(&self.limiter);
                limiter.should_exit_on_response(&response)
            };

            if rate_limited && attempt < MAX_RATE_LIMIT_RETRIES {
                attempt += 1;
                let status = response.status();
                let retry_after = Self::retry_after_duration(&response, attempt);
                log::warn!(
                    "Rate limited ({status}) for: {url}. Retry {attempt}/{} in {retry_after:?}",
                    MAX_RATE_LIMIT_RETRIES
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }

            {
                let mut limiter = Self::lock_limiter(&self.limiter);
                if limiter.should_exit_on_response(&response) {
                    let status = response.status();
                    let msg = format!(
                        "HTTP error {} for: {}. Handle limiter exit status reached.",
                        status, url
                    );
                    log::error!("{}", msg);
                    return Err(AdapterError::http_status_failed(status, msg));
                }

                limiter.update_from_response(&response, weight);
            }

            return Ok(response);
        }
    }

    /// Backoff for a rate-limited response. Prefers the server's `Retry-After`
    /// header (seconds form); falls back to exponential backoff.
    fn retry_after_duration(response: &Response, attempt: u32) -> Duration {
        if let Some(seconds) = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            return Duration::from_secs(seconds.saturating_add(1));
        }

        Duration::from_secs(2u64.saturating_pow(attempt).min(60))
    }

    /// Text-response layer.
    ///
    /// Builds on `http_response_with_limiter`, decodes the response body into UTF-8
    /// text, and emits enriched HTTP/status diagnostics on failure.
    pub(super) async fn http_text_with_limiter(
        &self,
        url: &str,
        weight: usize,
        method: Option<Method>,
        json_body: Option<&serde_json::Value>,
    ) -> Result<String, AdapterError> {
        let method = method.unwrap_or(Method::GET);

        let response = self
            .http_response_with_limiter(url, weight, method.clone(), json_body)
            .await?;

        Self::read_response_body(&method, url, response).await
    }

    /// JSON layer.
    ///
    /// Builds on `http_text_with_limiter`, validates response shape, and
    /// deserializes JSON into the target type with parse diagnostics.
    pub(super) async fn http_json_with_limiter<V>(
        &self,
        url: &str,
        weight: usize,
        method: Option<Method>,
        json_body: Option<&serde_json::Value>,
    ) -> Result<V, AdapterError>
    where
        V: DeserializeOwned,
    {
        let body = self
            .http_text_with_limiter(url, weight, method, json_body)
            .await?;
        Self::parse_json_body(url, &body)
    }

    async fn send_request_client(
        client: &Client,
        method: Method,
        url: &str,
        json_body: Option<&serde_json::Value>,
    ) -> Result<Response, reqwest::Error> {
        let mut request_builder = client.request(method, url);

        if let Some(body) = json_body {
            request_builder = request_builder.json(body);
        }

        request_builder.send().await
    }

    fn body_preview(body: &str, limit: usize) -> String {
        let trimmed = body.trim();
        let mut preview = trimmed.chars().take(limit).collect::<String>();

        if trimmed.chars().count() > limit {
            preview.push_str("...");
        }

        preview
    }

    fn parse_json_body<V>(url: &str, body: &str) -> Result<V, AdapterError>
    where
        V: DeserializeOwned,
    {
        let trimmed = body.trim();

        if trimmed.is_empty() {
            let msg = format!("Empty response body | url={url}");
            log::error!("{}", msg);
            return Err(AdapterError::ParseError(msg));
        }

        if trimmed.starts_with('<') {
            let msg = format!(
                "Non-JSON (HTML?) response | url={} | len={} | preview={:?}",
                url,
                body.len(),
                Self::body_preview(body, 200)
            );
            log::error!("{}", msg);
            return Err(AdapterError::ParseError(msg));
        }

        serde_json::from_str(body).map_err(|error| {
            let msg = format!(
                "JSON parse failed: {} | url={} | response_len={} | preview={:?}",
                error,
                url,
                body.len(),
                Self::body_preview(body, 200)
            );
            log::error!("{}", msg);
            AdapterError::ParseError(msg)
        })
    }

    async fn read_response_body(
        method: &Method,
        url: &str,
        response: Response,
    ) -> Result<String, AdapterError> {
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let body = response.bytes().await.map_err(|error| {
            AdapterError::response_body_failed(method, url, status, &content_type, error)
        })?;

        let body_text = String::from_utf8_lossy(&body).into_owned();

        if !status.is_success() {
            let msg = format!(
                "{} {}: HTTP {} | content-type={} | response_len={} | preview={:?}",
                method,
                url,
                status,
                content_type,
                body.len(),
                Self::body_preview(&body_text, 200)
            );
            log::error!("{}", msg);
            return Err(AdapterError::http_status_failed(status, msg));
        }

        Ok(body_text)
    }
}

pub(super) trait FetchCommandHandler<M> {
    const MAX_IN_FLIGHT_REQUESTS: usize = 8;

    fn fetch_ticker_metadata(
        &self,
        market_scope: M,
    ) -> BoxFuture<'_, Result<TickerMetadataMap, AdapterError>>;

    fn fetch_ticker_stats(
        &self,
        market_scope: M,
    ) -> BoxFuture<'_, Result<TickerStatsMap, AdapterError>>;

    fn fetch_klines(
        &self,
        ticker_info: TickerInfo,
        timeframe: Timeframe,
        range: Option<(UnixMs, UnixMs)>,
    ) -> BoxFuture<'_, Result<Vec<Kline>, AdapterError>> {
        let _ = (ticker_info, timeframe, range);
        Box::pin(async { Err(unsupported_fetch("Kline fetch")) })
    }

    fn fetch_open_interest(
        &self,
        ticker_info: TickerInfo,
        timeframe: Timeframe,
        range: Option<(UnixMs, UnixMs)>,
    ) -> BoxFuture<'_, Result<Vec<OpenInterest>, AdapterError>> {
        let _ = (ticker_info, timeframe, range);
        Box::pin(async { Err(unsupported_fetch("Open interest fetch")) })
    }

    fn fetch_depth_snapshot(
        &self,
        ticker: Ticker,
    ) -> BoxFuture<'_, Result<DepthPayload, AdapterError>> {
        let _ = ticker;
        Box::pin(async { Err(unsupported_fetch("Depth snapshot fetch")) })
    }

    fn fetch_trades(
        &self,
        ticker_info: TickerInfo,
        from_time: UnixMs,
        to_time: Option<UnixMs>,
        data_path: Option<PathBuf>,
    ) -> BoxFuture<'_, Result<Vec<Trade>, AdapterError>> {
        let _ = (ticker_info, from_time, to_time, data_path);
        Box::pin(async { Err(unsupported_fetch("Trades fetch")) })
    }
}

pub(super) fn spawn_fetch_worker<H, M>(worker: H) -> RequestPort<FetchCommand<M>>
where
    H: FetchCommandHandler<M> + Send + Sync + 'static,
    M: Send + 'static,
{
    const COMMAND_BUFFER_CAPACITY: usize = 128;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(COMMAND_BUFFER_CAPACITY);
    let worker = std::sync::Arc::new(worker);
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(H::MAX_IN_FLIGHT_REQUESTS));
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            let Ok(permit) = std::sync::Arc::clone(&slots).acquire_owned().await else {
                break;
            };
            let worker = std::sync::Arc::clone(&worker);
            tokio::spawn(async move {
                let _permit = permit;
                handle_fetch_command(worker.as_ref(), command).await;
            });
        }
    });
    RequestPort::new(sender)
}

fn unsupported_fetch(feature: &'static str) -> AdapterError {
    AdapterError::InvalidRequest(format!("{feature} is not supported by this worker"))
}

async fn handle_fetch_command<H, M>(handler: &H, command: FetchCommand<M>)
where
    H: FetchCommandHandler<M>,
{
    match command {
        FetchCommand::TickerMetadata {
            market_scope,
            reply,
        } => {
            let result = handler.fetch_ticker_metadata(market_scope).await;
            let _ = reply.send(result);
        }
        FetchCommand::TickerStats {
            market_scope,
            reply,
        } => {
            let result = handler.fetch_ticker_stats(market_scope).await;
            let _ = reply.send(result);
        }
        FetchCommand::Klines {
            ticker,
            timeframe,
            range,
            reply,
        } => {
            let result = handler.fetch_klines(ticker, timeframe, range).await;
            let _ = reply.send(result);
        }
        FetchCommand::OpenInterest {
            ticker,
            timeframe,
            range,
            reply,
        } => {
            let result = handler.fetch_open_interest(ticker, timeframe, range).await;
            let _ = reply.send(result);
        }
        FetchCommand::DepthSnapshot { ticker, reply } => {
            let result = handler.fetch_depth_snapshot(ticker).await;
            let _ = reply.send(result);
        }
        FetchCommand::Trades {
            ticker,
            from_time,
            to_time,
            data_path,
            reply,
        } => {
            let result = handler
                .fetch_trades(ticker, from_time, to_time, data_path)
                .await;
            let _ = reply.send(result);
        }
    }
}

pub(super) struct RequestPort<C> {
    sender: mpsc::Sender<C>,
}

impl<C> Clone for RequestPort<C> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<C> RequestPort<C> {
    pub(super) fn new(sender: mpsc::Sender<C>) -> Self {
        Self { sender }
    }

    pub(super) async fn request<T>(
        &self,
        build: impl FnOnce(ResponseTx<T>) -> C,
    ) -> Result<T, AdapterError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.sender
            .send(build(reply_tx))
            .await
            .map_err(|_| AdapterError::WebsocketError("Request port is closed".to_string()))?;

        reply_rx
            .await
            .map_err(|_| AdapterError::WebsocketError("Response channel dropped".to_string()))?
    }
}
