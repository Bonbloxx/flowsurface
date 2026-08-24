use exchange::adapter::{AdapterError, AdapterHandles, Exchange, StreamKind};
use exchange::{Kline, OpenInterest, TickerInfo, Trade, UnixMs};
use iced::{
    Task,
    futures::{FutureExt, future::BoxFuture, future::Shared},
    task::{Handle, Straw, sipper},
};
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;
use uuid::Uuid;

use crate::connector::{
    client::{DataSources, OiHistoryClient, ServerClient},
    open_interest,
};

pub use data::TradeFetchMode;

static TRADE_FETCH_MODE: RwLock<TradeFetchMode> = RwLock::new(TradeFetchMode::Off);

/// Maximum trades to request per Arrow IPC call to server.
const ARROW_LIMIT: usize = 400_000;

/// Keep historical trade ingestion responsive by yielding smaller work units
/// to the iced update loop. A busy UTC day can contain millions of trades;
/// processing large pages as one message blocks chart interaction.
const TRADE_UI_CHUNK: usize = 10_000;

fn split_trade_batch(batch: Vec<Trade>) -> Vec<Vec<Trade>> {
    if batch.len() <= TRADE_UI_CHUNK {
        vec![batch]
    } else {
        batch
            .chunks(TRADE_UI_CHUNK)
            .map(<[Trade]>::to_vec)
            .collect()
    }
}

/// Override the global trade-fetch mode at runtime.
pub fn set_trade_fetch_mode(mode: TradeFetchMode) {
    if let Ok(mut guard) = TRADE_FETCH_MODE.write() {
        *guard = mode;
    } else {
        log::error!("Trade fetch mode lock poisoned — resetting to Off");
    }
}

/// Return the current global trade-fetch mode.
pub fn trade_fetch_mode() -> TradeFetchMode {
    TRADE_FETCH_MODE
        .read()
        .map(|g| g.clone())
        .unwrap_or(TradeFetchMode::Off)
}

/// Returns `true` when a trade-fetch source (server or exchange API) is
/// active.
pub fn is_trade_fetch_enabled() -> bool {
    trade_fetch_mode() != TradeFetchMode::Off
}

#[derive(Debug, Clone)]
pub enum FetchedData {
    Trades {
        batch: Vec<Trade>,
        /// Optional request ID used to route every streamed page to the
        /// chart generation that requested it. Completion is delivered by
        /// [`FetchTaskStatus::Completed`], never inferred from a page's last
        /// timestamp: a server page can reach the upper bound before the
        /// fetcher fills an older exchange/archive gap.
        req_id: Option<Uuid>,
    },
    Klines {
        data: Vec<Kline>,
        req_id: Option<uuid::Uuid>,
    },
    OI {
        data: Vec<OpenInterest>,
        req_id: Option<uuid::Uuid>,
        /// Only the venue task is terminal. Remote history is delivered as an
        /// early, authoritative partial result while the serialized venue
        /// worker continues toward its latest observation.
        terminal: bool,
    },
}

/// Minimum wall-clock span for trade fetch windows. Live-edge bookkeeping can
/// produce gaps of a few milliseconds; each such request would otherwise cost
/// a full rate-limited REST page for near-zero data.
const MIN_TRADE_FETCH_WINDOW_MS: u64 = 1_000;

fn clamp_trade_window(fetch: FetchRange) -> FetchRange {
    let expand = |from: UnixMs, to: UnixMs| {
        let span = to.as_u64().saturating_sub(from.as_u64());
        if span >= MIN_TRADE_FETCH_WINDOW_MS {
            (from, to)
        } else {
            (from.saturating_sub(MIN_TRADE_FETCH_WINDOW_MS - span), to)
        }
    };

    match fetch {
        FetchRange::Trades(from, to) => {
            let (from, to) = expand(from, to);
            FetchRange::Trades(from, to)
        }
        FetchRange::FootprintTrades(from, to) => {
            let (from, to) = expand(from, to);
            FetchRange::FootprintTrades(from, to)
        }
        FetchRange::TradesRecent(from, to) => {
            let (from, to) = expand(from, to);
            FetchRange::TradesRecent(from, to)
        }
        other => other,
    }
}

fn range_contains(
    (outer_from, outer_to): (UnixMs, UnixMs),
    (inner_from, inner_to): (UnixMs, UnixMs),
) -> bool {
    outer_from <= inner_from && outer_to >= inner_to
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum ReqError {
    #[error("Request overlaps with an existing request")]
    Overlaps,
    #[error("Source has no data for the requested range")]
    NoData,
    /// A previous attempt for this range failed and the retry cooldown
    /// has not yet elapsed.
    #[error("Previous request failed, retry not yet allowed")]
    Failed,
}

/// Lifecycle of a fetch request.
///
/// * `Failed` is retried after the cooldown (transient errors may resolve).
///   After [`RequestHandler::MAX_FETCH_ATTEMPTS`] the request is treated as
///   `NoData` so a permanently broken range cannot retry forever.
/// * `Completed` and `NoData` are never retried — the data is either
///   already present or the source confirmed the range is empty.
#[derive(PartialEq, Clone, Debug)]
enum RequestStatus {
    Pending,
    /// Fetch succeeded and data was inserted.
    Completed,
    /// The source returned an empty result for this range.
    NoData,
    /// The fetch failed (network error, parse error, etc.).
    Failed {
        at: u64,
        attempts: u32,
    },
}

#[derive(Default)]
pub struct RequestHandler {
    requests: FxHashMap<Uuid, FetchRequest>,
}

impl RequestHandler {
    const RETRY_AFTER_MS: u64 = 30_000;
    const MAX_FETCH_ATTEMPTS: u32 = 5;

    pub fn add_request(
        &mut self,
        fetch: FetchRange,
        stream: Option<StreamKind>,
    ) -> Result<Option<Uuid>, ReqError> {
        let request = FetchRequest::new(clamp_trade_window(fetch), stream);
        let id = Uuid::new_v4();

        if let Some((existing_id, existing_req)) = self.requests.iter_mut().find_map(|(k, v)| {
            if v.same_with(&request) {
                Some((*k, v))
            } else {
                None
            }
        }) {
            let now_ms = chrono::Utc::now().timestamp_millis() as u64;
            let retry_after_ms = Self::RETRY_AFTER_MS;
            let status = existing_req.status.clone();

            return match status {
                RequestStatus::Completed => Ok(None),
                RequestStatus::Pending => Err(ReqError::Overlaps),
                RequestStatus::NoData => Err(ReqError::NoData),
                RequestStatus::Failed { at: _, attempts }
                    if attempts >= Self::MAX_FETCH_ATTEMPTS =>
                {
                    Err(ReqError::NoData)
                }
                RequestStatus::Failed { at, .. } => {
                    if now_ms - at > retry_after_ms {
                        existing_req.status = RequestStatus::Pending;
                        Ok(Some(existing_id))
                    } else {
                        Err(ReqError::Failed)
                    }
                }
            };
        }

        // A range already covered by a tracked request normally must not burn
        // another rate-limited page. Visible footprint history is the exception:
        // a broad server response can span an internal recorder outage, and the
        // chart deliberately follows it with a narrower request for the proven
        // empty candle range. That second pass is what lets the fetcher fall back
        // to exchange/archive data. Exact completed ranges remain suppressed.
        let mut completed_coverage = false;
        for existing in self
            .requests
            .values()
            .filter(|existing| existing.contains(&request))
        {
            match existing.status {
                RequestStatus::Pending => return Err(ReqError::Overlaps),
                RequestStatus::NoData => return Err(ReqError::NoData),
                RequestStatus::Completed => {
                    if !existing.allows_completed_footprint_subrange(&request) {
                        completed_coverage = true;
                    }
                }
                RequestStatus::Failed { .. } => {}
            }
        }
        if completed_coverage {
            return Ok(None);
        }

        self.requests.insert(id, request);
        Ok(Some(id))
    }

    pub fn mark_completed(&mut self, id: Uuid) {
        if let Some(request) = self.requests.get_mut(&id) {
            request.status = RequestStatus::Completed;
        } else {
            log::warn!("Request not found: {:?}", id);
        }
    }

    /// Forget a tracked request entirely.
    ///
    /// Used when a registered request could not be dispatched (for example a
    /// kline fetch planned before any kline stream was ready): leaving it
    /// `Pending` would suppress every retry of that range as an overlap while
    /// the data never arrives.
    pub fn remove(&mut self, id: Uuid) {
        self.requests.remove(&id);
    }

    /// Mark a request as completed with no data — the source returned an
    /// empty result.  The range will never be retried.
    pub fn mark_no_data(&mut self, id: Uuid) {
        if let Some(request) = self.requests.get_mut(&id) {
            request.status = RequestStatus::NoData;
        } else {
            log::warn!("Request not found: {:?}", id);
        }
    }

    /// Drop kline/trade gap-fill requests but keep Footprint History backfills.
    /// Timeframe and tick-size changes must not re-download UTC-day trades.
    pub fn drop_non_footprint_history(&mut self) {
        self.requests.retain(|_, request| {
            matches!(
                request.fetch_type,
                FetchRange::FootprintHistoryTrades(..)
                    | FetchRange::FootprintHistoryOpenInterest(..)
            )
        });
    }

    /// Drop every tracked request issued on `stream`.
    ///
    /// Used when a seeded bar store goes stale (for example Previous Value
    /// Areas after a UTC-day rollover): the completed ranges would otherwise
    /// suppress the top-up fetches that cover the newly elapsed time.
    pub fn drop_requests_for_stream(&mut self, stream: StreamKind) {
        self.requests
            .retain(|_, request| request.stream != Some(stream));
    }

    pub fn mark_failed(&mut self, id: Uuid) {
        if let Some(request) = self.requests.get_mut(&id) {
            let timestamp = chrono::Utc::now().timestamp_millis() as u64;
            let attempts = match &request.status {
                RequestStatus::Failed { attempts, .. } => attempts + 1,
                _ => 1,
            };
            request.status = RequestStatus::Failed {
                at: timestamp,
                attempts,
            };
            log::debug!(
                "Fetch request failed (attempt {attempts}/{}): {:?}",
                Self::MAX_FETCH_ATTEMPTS,
                request.fetch_type
            );
        } else {
            log::warn!("Request not found: {:?}", id);
        }
    }
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum FetchRange {
    Kline(UnixMs, UnixMs),
    OpenInterest(UnixMs, UnixMs),
    Trades(UnixMs, UnixMs),
    /// Complete executed-trade history for a visible Footprint chart range.
    /// Unlike generic seeds, this must not stop at a fixed trade-count cap.
    FootprintTrades(UnixMs, UnixMs),
    /// Per-UTC-day trades requested by Footprint History / Daily Delta.
    FootprintHistoryTrades(UnixMs, UnixMs),
    /// Open-interest history requested exclusively by Footprint History.
    FootprintHistoryOpenInterest(UnixMs, UnixMs),
    /// A trade window where the newest contiguous data is more important than
    /// starting exactly at the requested lower bound (for example, Renko seed).
    TradesRecent(UnixMs, UnixMs),
}

#[derive(PartialEq, Debug)]
struct FetchRequest {
    fetch_type: FetchRange,
    stream: Option<StreamKind>,
    status: RequestStatus,
}

impl FetchRequest {
    fn new(fetch_type: FetchRange, stream: Option<StreamKind>) -> Self {
        FetchRequest {
            fetch_type,
            stream,
            status: RequestStatus::Pending,
        }
    }

    fn same_with(&self, other: &FetchRequest) -> bool {
        self.stream == other.stream && self.same_with_range(&other.fetch_type)
    }

    /// Check whether the stored [`FetchRange`] matches a given range.
    fn same_with_range(&self, range: &FetchRange) -> bool {
        match (&self.fetch_type, range) {
            (FetchRange::Kline(s1, e1), FetchRange::Kline(s2, e2)) => e1 == e2 && s1 == s2,
            (FetchRange::OpenInterest(s1, e1), FetchRange::OpenInterest(s2, e2)) => {
                e1 == e2 && s1 == s2
            }
            (FetchRange::Trades(s1, e1), FetchRange::Trades(s2, e2)) => e1 == e2 && s1 == s2,
            (FetchRange::FootprintTrades(s1, e1), FetchRange::FootprintTrades(s2, e2)) => {
                e1 == e2 && s1 == s2
            }
            (
                FetchRange::FootprintHistoryTrades(s1, e1),
                FetchRange::FootprintHistoryTrades(s2, e2),
            ) => e1 == e2 && s1 == s2,
            (
                FetchRange::FootprintHistoryOpenInterest(s1, e1),
                FetchRange::FootprintHistoryOpenInterest(s2, e2),
            ) => e1 == e2 && s1 == s2,
            (FetchRange::TradesRecent(s1, e1), FetchRange::TradesRecent(s2, e2)) => {
                e1 == e2 && s1 == s2
            }
            _ => false,
        }
    }

    /// Check whether the stored [`FetchRange`] fully covers a given range of
    /// the same kind. Used to skip redundant sub-range requests.
    fn contains(&self, other: &FetchRequest) -> bool {
        if self.stream != other.stream {
            return false;
        }
        let bounds = |range: &FetchRange| match range {
            FetchRange::Kline(from, to)
            | FetchRange::OpenInterest(from, to)
            | FetchRange::Trades(from, to)
            | FetchRange::FootprintTrades(from, to)
            | FetchRange::FootprintHistoryTrades(from, to)
            | FetchRange::FootprintHistoryOpenInterest(from, to)
            | FetchRange::TradesRecent(from, to) => Some((*from, *to)),
        };
        let (Some(outer), Some(inner)) = (bounds(&self.fetch_type), bounds(&other.fetch_type))
        else {
            return false;
        };
        std::mem::discriminant(&self.fetch_type) == std::mem::discriminant(&other.fetch_type)
            && range_contains(outer, inner)
    }

    fn allows_completed_footprint_subrange(&self, other: &FetchRequest) -> bool {
        matches!(
            (&self.fetch_type, &other.fetch_type),
            (
                FetchRange::FootprintTrades(outer_from, outer_to),
                FetchRange::FootprintTrades(inner_from, inner_to),
            ) if (outer_from, outer_to) != (inner_from, inner_to)
        )
    }
}

pub struct FetchSpec {
    pub req_id: uuid::Uuid,
    pub fetch: FetchRange,
    pub stream: Option<StreamKind>,
}

impl From<(uuid::Uuid, FetchRange, Option<StreamKind>)> for FetchSpec {
    fn from(t: (uuid::Uuid, FetchRange, Option<StreamKind>)) -> Self {
        FetchSpec {
            req_id: t.0,
            fetch: t.1,
            stream: t.2,
        }
    }
}

impl std::fmt::Debug for FetchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchSpec")
            .field("req_id", &self.req_id)
            .field("fetch", &self.fetch)
            .field("stream", &self.stream)
            .finish()
    }
}

impl Clone for FetchSpec {
    fn clone(&self) -> Self {
        FetchSpec {
            req_id: self.req_id,
            fetch: self.fetch,
            stream: self.stream,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// Used for showing updates in pane title bar.
pub enum FetchTaskStatus {
    Loading(InfoKind),
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InfoKind {
    FetchingKlines,
    FetchingTrades(usize),
    FetchingOI,
}

#[derive(Debug, Clone)]
pub enum FetchUpdate {
    Status {
        pane_id: Uuid,
        status: FetchTaskStatus,
        /// Present for trade fetches so the chart can finalize the request.
        req_id: Option<Uuid>,
    },
    Data {
        layout_id: Uuid,
        pane_id: Uuid,
        stream: StreamKind,
        data: FetchedData,
    },
    Error {
        pane_id: Uuid,
        error: String,
        req_id: Option<Uuid>,
    },
}

pub fn request_fetch(
    sources: &DataSources,
    pane_id: Uuid,
    ready_streams: &[StreamKind],
    layout_id: Uuid,
    req_id: Uuid,
    fetch: FetchRange,
    stream: Option<StreamKind>,
    on_trade_handle: &mut impl FnMut(Handle),
) -> Task<FetchUpdate> {
    let handles = sources.exchange.clone();

    match fetch {
        FetchRange::Kline(from, to) => {
            let kline_stream = if let Some(s) = stream {
                Some((s, pane_id))
            } else {
                ready_streams.iter().find_map(|stream| {
                    if let StreamKind::Kline { .. } = stream {
                        Some((*stream, pane_id))
                    } else {
                        None
                    }
                })
            };

            if let Some((stream, pane_uid)) = kline_stream {
                return kline_fetch_task(
                    handles,
                    layout_id,
                    pane_uid,
                    stream,
                    Some(req_id),
                    Some((from, to)),
                );
            }
        }
        FetchRange::OpenInterest(from, to) | FetchRange::FootprintHistoryOpenInterest(from, to) => {
            let kline_stream = if let Some(s) = stream {
                Some((s, pane_id))
            } else {
                ready_streams.iter().find_map(|stream| {
                    if let StreamKind::Kline { .. } = stream {
                        Some((*stream, pane_id))
                    } else {
                        None
                    }
                })
            };

            if let Some((stream, pane_uid)) = kline_stream {
                return oi_fetch_task(
                    handles.clone(),
                    sources.oi_history.clone(),
                    layout_id,
                    pane_uid,
                    stream,
                    Some(req_id),
                    Some((from, to)),
                );
            }
        }
        FetchRange::Trades(from_time, to_time)
        | FetchRange::FootprintTrades(from_time, to_time)
        | FetchRange::FootprintHistoryTrades(from_time, to_time)
        | FetchRange::TradesRecent(from_time, to_time) => {
            let recent_first = matches!(fetch, FetchRange::TradesRecent(..));
            let trade_stream = select_trade_stream(stream, ready_streams);
            let trade_info = trade_stream.map(|stream| (stream.ticker_info(), pane_id, stream));

            if let Some((ticker_info, pane_id, stream)) = trade_info {
                let supports_exchange_fetch = matches!(
                    ticker_info.exchange(),
                    Exchange::BinanceSpot
                        | Exchange::BinanceLinear
                        | Exchange::BinanceInverse
                        | Exchange::BybitLinear
                        | Exchange::BybitInverse
                );
                let mode = trade_fetch_mode();
                let server = sources.server.clone();
                let data_path = match ticker_info.exchange().venue() {
                    exchange::adapter::Venue::Binance => {
                        data::data_path(Some("market_data/binance/"))
                    }
                    exchange::adapter::Venue::Bybit => data::data_path(Some("market_data/bybit/")),
                    _ => data::data_path(Some("market_data/")),
                };

                if let Some(ref client) = server {
                    log::trace!(
                        "Trade fetch: using server at {} ({})",
                        client.base_url(),
                        ticker_info.exchange()
                    );
                } else if mode == TradeFetchMode::Server {
                    log::error!(
                        "Server mode selected but server URL is invalid, check Network Manager settings"
                    );
                    return Task::done(FetchUpdate::Error {
                        pane_id,
                        error: "Server mode selected but the server URL is invalid.".to_string(),
                        req_id: Some(req_id),
                    });
                } else if supports_exchange_fetch {
                    log::debug!(
                        "Trade fetch: using direct exchange API for {}",
                        ticker_info.exchange()
                    );
                } else {
                    log::warn!(
                        "Trade fetch via exchange API is only supported for Binance and Bybit, got {}",
                        ticker_info.exchange()
                    );
                    return Task::done(FetchUpdate::Error {
                        pane_id,
                        error: format!(
                            "Trade fetch via exchange API is only supported for Binance and Bybit, got {}",
                            ticker_info.exchange()
                        ),
                        req_id: Some(req_id),
                    });
                }

                let is_complete_footprint = matches!(
                    fetch,
                    FetchRange::FootprintTrades(..) | FetchRange::FootprintHistoryTrades(..)
                );
                let (task, handle) = Task::sip(
                    fetch_trades_paged(
                        server,
                        handles,
                        ticker_info,
                        from_time,
                        to_time,
                        data_path,
                        recent_first,
                        // A completed UTC-day profile must be complete. Busy symbols can
                        // legitimately exceed any fixed trade-count ceiling, so Footprint
                        // History / Daily Delta page until the requested boundary. Generic
                        // chart seeds stay bounded because they only need a useful window.
                        trade_fetch_limits(is_complete_footprint),
                        is_complete_footprint,
                    ),
                    move |batch| {
                        let data = FetchedData::Trades {
                            batch,
                            req_id: Some(req_id),
                        };

                        FetchUpdate::Data {
                            layout_id,
                            pane_id,
                            data,
                            stream,
                        }
                    },
                    move |result| match result {
                        Ok(true) => FetchUpdate::Status {
                            pane_id,
                            status: FetchTaskStatus::Completed,
                            req_id: Some(req_id),
                        },
                        Ok(false) => {
                            // Source returned no data for this range. Produce
                            // an empty batch so the dashboard calls mark_no_data.
                            FetchUpdate::Data {
                                layout_id,
                                pane_id,
                                data: FetchedData::Trades {
                                    batch: Vec::new(),
                                    req_id: Some(req_id),
                                },
                                stream,
                            }
                        }
                        Err(err) => {
                            log::error!("Trade fetch failed: {err}");
                            FetchUpdate::Error {
                                pane_id,
                                error: err.ui_message(),
                                req_id: Some(req_id),
                            }
                        }
                    },
                )
                .abortable();

                on_trade_handle(handle.abort_on_drop());

                return task;
            }
        }
    }

    Task::none()
}

fn select_trade_stream(
    explicit: Option<StreamKind>,
    ready_streams: &[StreamKind],
) -> Option<StreamKind> {
    explicit
        .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
        .or_else(|| {
            ready_streams
                .iter()
                .copied()
                .find(|stream| matches!(stream, StreamKind::Trades { .. }))
        })
}

/// Whether [`request_fetch`] can produce a real task for this spec right now.
///
/// Kline/OI ranges need a ready kline stream and trade ranges a ready trade
/// stream when no explicit stream is attached. A pane may plan fetches before
/// its streams resolve (ticker metadata still loading); those specs must be
/// reported back instead of silently registering as `Pending` forever.
fn fetch_is_dispatchable(
    fetch: &FetchRange,
    stream: Option<StreamKind>,
    ready_streams: &[StreamKind],
) -> bool {
    match fetch {
        FetchRange::Kline(..)
        | FetchRange::OpenInterest(..)
        | FetchRange::FootprintHistoryOpenInterest(..) => {
            let explicit_kline =
                stream.is_some_and(|stream| matches!(stream, StreamKind::Kline { .. }));
            explicit_kline
                || ready_streams
                    .iter()
                    .any(|stream| matches!(stream, StreamKind::Kline { .. }))
        }
        FetchRange::Trades(..)
        | FetchRange::FootprintTrades(..)
        | FetchRange::FootprintHistoryTrades(..)
        | FetchRange::TradesRecent(..) => select_trade_stream(stream, ready_streams).is_some(),
    }
}

pub fn request_fetch_many(
    sources: &DataSources,
    pane_id: Uuid,
    ready_streams: &[StreamKind],
    layout_id: Uuid,
    reqs: impl IntoIterator<Item = (Uuid, FetchRange, Option<StreamKind>)>,
    mut on_trade_handle: impl FnMut(Uuid, Handle),
) -> (Task<FetchUpdate>, Vec<Uuid>) {
    let mut tasks = Vec::new();
    let mut undispatched = Vec::new();

    for (req_id, fetch, stream) in reqs {
        if !fetch_is_dispatchable(&fetch, stream, ready_streams) {
            log::debug!(
                "Fetch request {req_id} has no ready stream yet; releasing for retry: {fetch:?}"
            );
            undispatched.push(req_id);
            continue;
        }
        let mut retain_handle = |handle| on_trade_handle(req_id, handle);
        tasks.push(request_fetch(
            sources,
            pane_id,
            ready_streams,
            layout_id,
            req_id,
            fetch,
            stream,
            &mut retain_handle,
        ));
    }

    (Task::batch(tasks), undispatched)
}

pub fn oi_fetch_task(
    handles: AdapterHandles,
    oi_history: Option<OiHistoryClient>,
    layout_id: Uuid,
    pane_id: Uuid,
    stream: StreamKind,
    req_id: Option<Uuid>,
    range: Option<(UnixMs, UnixMs)>,
) -> Task<FetchUpdate> {
    let update_status = Task::done(FetchUpdate::Status {
        pane_id,
        status: FetchTaskStatus::Loading(InfoKind::FetchingOI),
        req_id,
    });

    let fetch_task = match stream {
        StreamKind::Kline {
            ticker_info,
            timeframe,
        } => {
            let venue_started_at = Instant::now();
            let venue_fetch = async move {
                match handles
                    .fetch_open_interest(ticker_info, timeframe, range)
                    .await
                {
                    Ok(fetched) => Ok(open_interest::merge_and_load(ticker_info, &fetched, range)),
                    Err(err) => oi_archive_fallback(ticker_info, range, err),
                }
            };
            let venue_task = Task::perform(
                iced::futures::TryFutureExt::map_err(venue_fetch, |err| {
                    log::error!("Open interest fetch failed: {err}");
                    err.ui_message()
                }),
                move |result| match result {
                    Ok(oi) => {
                        log::debug!(
                            "history_load kind=oi source=venue exchange={} ticker={} timeframe={timeframe:?} range={range:?} elapsed_ms={} points={}",
                            ticker_info.exchange(),
                            ticker_info.ticker,
                            venue_started_at.elapsed().as_millis(),
                            oi.len()
                        );
                        let data = FetchedData::OI {
                            data: oi,
                            req_id,
                            terminal: true,
                        };
                        FetchUpdate::Data {
                            layout_id,
                            pane_id,
                            data,
                            stream,
                        }
                    }
                    Err(err) => {
                        log::debug!(
                            "history_load kind=oi source=venue exchange={} ticker={} timeframe={timeframe:?} range={range:?} elapsed_ms={} error={err}",
                            ticker_info.exchange(),
                            ticker_info.ticker,
                            venue_started_at.elapsed().as_millis()
                        );
                        FetchUpdate::Error {
                            pane_id,
                            error: err,
                            req_id,
                        }
                    }
                },
            );

            let mut tasks = vec![venue_task];
            if let (Some(client), Some((from, to))) = (oi_history, range) {
                let remote_started_at = Instant::now();
                let remote_fetch = async move {
                    match client.fetch_open_interest(ticker_info, from, to).await {
                        Ok(fetched) => open_interest::merge_and_load(ticker_info, &fetched, range),
                        Err(err) => {
                            log::warn!(
                                "OI history service failed for {}; venue fetch remains active: {err}",
                                ticker_info.ticker
                            );
                            Vec::new()
                        }
                    }
                };
                let remote_task = Task::perform(remote_fetch, move |oi| {
                    log::debug!(
                        "history_load kind=oi source=remote exchange={} ticker={} timeframe={timeframe:?} range={range:?} elapsed_ms={} points={}",
                        ticker_info.exchange(),
                        ticker_info.ticker,
                        remote_started_at.elapsed().as_millis(),
                        oi.len()
                    );
                    FetchUpdate::Data {
                        layout_id,
                        pane_id,
                        data: FetchedData::OI {
                            data: oi,
                            req_id,
                            terminal: false,
                        },
                        stream,
                    }
                });
                tasks.insert(0, remote_task);
            }
            Task::batch(tasks)
        }
        _ => Task::none(),
    };

    update_status.chain(fetch_task)
}

#[cfg(test)]
fn merge_open_interest_observations(
    exchange: &[OpenInterest],
    remote: &[OpenInterest],
) -> Vec<OpenInterest> {
    const MINUTE_MS: u64 = 60_000;

    let mut by_minute = std::collections::BTreeMap::<u64, OpenInterest>::new();
    for observation in exchange.iter().chain(remote) {
        let minute = observation.time.as_u64() / MINUTE_MS * MINUTE_MS;
        let replace = by_minute
            .get(&minute)
            .is_none_or(|current| observation.time >= current.time);
        if replace {
            by_minute.insert(minute, *observation);
        }
    }
    by_minute.into_values().collect()
}

fn oi_archive_fallback(
    ticker_info: TickerInfo,
    range: Option<(UnixMs, UnixMs)>,
    network_error: AdapterError,
) -> Result<Vec<OpenInterest>, AdapterError> {
    let archived = open_interest::load_range(ticker_info, range);
    if archived.is_empty() {
        Err(network_error)
    } else {
        log::warn!(
            "Open interest network fetch failed for {}; using {} archived samples: {}",
            ticker_info.ticker,
            archived.len(),
            network_error
        );
        Ok(archived)
    }
}

pub fn kline_fetch_task(
    handles: AdapterHandles,
    layout_id: Uuid,
    pane_id: Uuid,
    stream: StreamKind,
    req_id: Option<Uuid>,
    range: Option<(UnixMs, UnixMs)>,
) -> Task<FetchUpdate> {
    let update_status = Task::done(FetchUpdate::Status {
        pane_id,
        status: FetchTaskStatus::Loading(InfoKind::FetchingKlines),
        req_id,
    });

    let fetch_task = match stream {
        StreamKind::Kline {
            ticker_info,
            timeframe,
        } => {
            let started_at = Instant::now();
            let (fetch, coalesced) = shared_kline_fetch(handles, ticker_info, timeframe, range);

            Task::perform(
                iced::futures::TryFutureExt::map_err(fetch, |err| {
                    log::error!("Kline fetch failed: {err}");
                    err.ui_message()
                }),
                move |result| match result {
                    Ok(klines) => {
                        log::debug!(
                            "history_load kind=kline pane={pane_id} exchange={} ticker={} timeframe={timeframe:?} range={range:?} elapsed_ms={} points={} coalesced={coalesced}",
                            ticker_info.exchange(),
                            ticker_info.ticker,
                            started_at.elapsed().as_millis(),
                            klines.len()
                        );
                        let data = FetchedData::Klines {
                            data: (*klines).clone(),
                            req_id,
                        };
                        FetchUpdate::Data {
                            layout_id,
                            pane_id,
                            data,
                            stream,
                        }
                    }
                    Err(err) => {
                        log::debug!(
                            "history_load kind=kline pane={pane_id} exchange={} ticker={} timeframe={timeframe:?} range={range:?} elapsed_ms={} error={err}",
                            ticker_info.exchange(),
                            ticker_info.ticker,
                            started_at.elapsed().as_millis()
                        );
                        FetchUpdate::Error {
                            pane_id,
                            error: err,
                            req_id,
                        }
                    }
                },
            )
        }
        _ => Task::none(),
    };

    update_status.chain(fetch_task)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct KlineFetchKey {
    ticker_info: TickerInfo,
    timeframe: exchange::Timeframe,
    range: Option<(UnixMs, UnixMs)>,
}

type SharedKlineFetch = Shared<BoxFuture<'static, Result<Arc<Vec<Kline>>, Arc<AdapterError>>>>;

fn in_flight_kline_fetches() -> &'static Mutex<FxHashMap<KlineFetchKey, SharedKlineFetch>> {
    static FETCHES: OnceLock<Mutex<FxHashMap<KlineFetchKey, SharedKlineFetch>>> = OnceLock::new();
    FETCHES.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Fan out one exact venue response to every pane requesting the same immutable
/// source/timeframe/range. The entry exists only while the HTTP request is in
/// flight, so this cannot serve stale market data.
fn shared_kline_fetch(
    handles: AdapterHandles,
    ticker_info: TickerInfo,
    timeframe: exchange::Timeframe,
    range: Option<(UnixMs, UnixMs)>,
) -> (SharedKlineFetch, bool) {
    let key = KlineFetchKey {
        ticker_info,
        timeframe,
        range,
    };
    let mut fetches = in_flight_kline_fetches()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(fetch) = fetches.get(&key) {
        return (fetch.clone(), true);
    }

    let fetch = async move {
        let result = handles
            .fetch_klines(ticker_info, timeframe, range)
            .await
            .map(Arc::new)
            .map_err(Arc::new);
        in_flight_kline_fetches()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        result
    }
    .boxed()
    .shared();
    fetches.insert(key, fetch.clone());
    (fetch, false)
}

/// Fetch trades from the configured source using a single forward-paging
/// loop (oldest → newest).
///
/// Returns `Ok(true)` when at least one trade was received, `Ok(false)`
/// when the source confirmed the range has no data.
fn fetch_trades_paged(
    server: Option<ServerClient>,
    handles: AdapterHandles,
    ticker_info: TickerInfo,
    from_time: UnixMs,
    to_time: UnixMs,
    data_path: PathBuf,
    recent_first: bool,
    limits: Option<FetchLimits>,
    fill_exchange_gaps: bool,
) -> impl Straw<bool, Vec<Trade>, AdapterError> {
    sipper(async move |mut progress| {
        if recent_first {
            let batches = if let Some(client) = server {
                fetch_recent_server_trade_batches(client, ticker_info, from_time, to_time).await?
            } else {
                fetch_recent_exchange_trade_batches(
                    handles,
                    ticker_info,
                    from_time,
                    to_time,
                    data_path,
                )
                .await?
            };
            let had_data = !batches.is_empty();
            for batch in batches {
                progress.send(batch).await;
            }
            return Ok(had_data);
        }

        let mut cursor = from_time;
        let mut had_data = false;
        let mut pages: usize = 0;
        let mut total_trades: usize = 0;
        let mut earliest_server: Option<UnixMs> = None;
        let exchange_available = supports_exchange_trade_fetch(ticker_info);

        let query_server = if let Some(ref client) = server {
            match client.earliest_trade_time(ticker_info).await {
                Ok(earliest) => server_range_may_have_data(to_time, earliest),
                Err(err) => {
                    log::debug!(
                        "Could not read server coverage for {} ({}): {err}",
                        ticker_info.ticker,
                        ticker_info.exchange()
                    );
                    true
                }
            }
        } else {
            false
        };

        if query_server && let Some(ref client) = server {
            while cursor <= to_time {
                if fetch_limit_reached(limits, pages, total_trades) {
                    log::info!(
                        "Trade fetch page/trade cap reached (pages={pages}, trades={total_trades}); stopping seed"
                    );
                    break;
                }
                pages += 1;

                let prev_cursor = cursor;
                let parsed = client
                    .fetch_trades_arrow(ticker_info, cursor, to_time, ARROW_LIMIT)
                    .await?;

                let is_empty = parsed.raw_row_count == 0;
                if is_empty {
                    break;
                }
                if let Some(first) = parsed.trades.first() {
                    earliest_server = Some(match earliest_server {
                        Some(current) => current.min(first.time),
                        None => first.time,
                    });
                }
                if !parsed.trades.is_empty() {
                    had_data = true;
                    total_trades = total_trades.saturating_add(parsed.trades.len());
                    for chunk in split_trade_batch(parsed.trades) {
                        progress.send(chunk).await;
                    }
                }
                cursor = parsed.last_ts.map_or(cursor, |t| t.saturating_add(1));
                if cursor <= prev_cursor {
                    log::error!(
                        "paging cursor did not advance past {prev_cursor:?} despite a non-empty response; \
                 aborting to avoid an infinite loop (source may be malformed)"
                    );
                    return Err(AdapterError::ParseError(
                        "Source may be malformed. Check logs for details.".to_string(),
                    ));
                }
            }
        }

        let exchange_ranges = exchange_trade_ranges(
            server.is_some(),
            fill_exchange_gaps,
            from_time,
            to_time,
            earliest_server,
            cursor,
            had_data,
        );

        for (exchange_from, exchange_to) in exchange_ranges {
            if !exchange_available {
                break;
            }
            cursor = exchange_from;
            while cursor <= exchange_to {
                if fetch_limit_reached(limits, pages, total_trades) {
                    log::info!(
                        "Trade fetch page/trade cap reached (pages={pages}, trades={total_trades}); stopping seed"
                    );
                    break;
                }
                pages += 1;

                let prev_cursor = cursor;
                let mut batch = handles
                    .fetch_trades(
                        ticker_info,
                        cursor,
                        Some(exchange_to),
                        Some(data_path.clone()),
                    )
                    .await?;
                batch.retain(|trade| trade.time >= from_time && trade.time <= exchange_to);
                if batch.is_empty() {
                    break;
                }

                had_data = true;
                batch.sort_by_key(|trade| trade.time);
                cursor = batch.last().map_or(cursor, |t| t.time.saturating_add(1));
                total_trades = total_trades.saturating_add(batch.len());
                for chunk in split_trade_batch(batch) {
                    progress.send(chunk).await;
                }

                if cursor <= prev_cursor {
                    log::error!(
                        "paging cursor did not advance past {prev_cursor:?} despite a non-empty response; \
                 aborting to avoid an infinite loop (source may be malformed)"
                    );
                    return Err(AdapterError::ParseError(
                        "Source may be malformed. Check logs for details.".to_string(),
                    ));
                }
            }
        }

        Ok(had_data)
    })
}

fn exchange_trade_ranges(
    server_configured: bool,
    fill_exchange_gaps: bool,
    from_time: UnixMs,
    to_time: UnixMs,
    earliest_server: Option<UnixMs>,
    next_server_cursor: UnixMs,
    had_server_data: bool,
) -> Vec<(UnixMs, UnixMs)> {
    if !server_configured {
        return vec![(from_time, to_time)];
    }
    if !fill_exchange_gaps {
        return Vec::new();
    }
    if !had_server_data {
        return vec![(from_time, to_time)];
    }

    let mut ranges = Vec::with_capacity(2);
    if let Some(first) = earliest_server
        && first > from_time
    {
        ranges.push((from_time, first.saturating_sub(1)));
    }
    if next_server_cursor < to_time {
        ranges.push((next_server_cursor, to_time));
    }
    ranges
}

fn server_range_may_have_data(to_time: UnixMs, earliest_server: Option<UnixMs>) -> bool {
    earliest_server.is_none_or(|earliest| to_time >= earliest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FetchLimits {
    max_total_trades: usize,
    max_pages: usize,
}

fn trade_fetch_limits(is_footprint_history: bool) -> Option<FetchLimits> {
    (!is_footprint_history).then_some(FetchLimits {
        max_total_trades: 80_000,
        max_pages: 40,
    })
}

fn fetch_limit_reached(limits: Option<FetchLimits>, pages: usize, total_trades: usize) -> bool {
    limits
        .is_some_and(|limits| pages >= limits.max_pages || total_trades >= limits.max_total_trades)
}

fn supports_exchange_trade_fetch(ticker_info: TickerInfo) -> bool {
    matches!(
        ticker_info.exchange(),
        Exchange::BinanceSpot
            | Exchange::BinanceLinear
            | Exchange::BinanceInverse
            | Exchange::BybitLinear
            | Exchange::BybitInverse
    )
}

async fn fetch_recent_exchange_trade_batches(
    handles: AdapterHandles,
    ticker_info: TickerInfo,
    from_time: UnixMs,
    to_time: UnixMs,
    data_path: PathBuf,
) -> Result<Vec<Vec<Trade>>, AdapterError> {
    const MAX_PAGES: usize = 40;
    const MAX_TOTAL_TRADES: usize = 80_000;

    let mut cursor = to_time;
    let mut total_trades = 0usize;
    let mut newest_to_oldest = Vec::new();

    while cursor >= from_time
        && newest_to_oldest.len() < MAX_PAGES
        && total_trades < MAX_TOTAL_TRADES
    {
        let mut batch = handles
            .fetch_trades(
                ticker_info,
                from_time,
                Some(cursor),
                Some(data_path.clone()),
            )
            .await?;
        batch.retain(|trade| trade.time >= from_time && trade.time <= cursor);
        if batch.is_empty() {
            break;
        }

        batch.sort_by_key(|trade| trade.time);
        let oldest = batch.first().map_or(cursor, |trade| trade.time);
        total_trades = total_trades.saturating_add(batch.len());
        newest_to_oldest.push(batch);

        let next_cursor = oldest.saturating_sub(1);
        if next_cursor >= cursor {
            return Err(AdapterError::ParseError(
                "Recent trade paging cursor did not move backward".to_string(),
            ));
        }
        cursor = next_cursor;
    }

    newest_to_oldest.reverse();
    Ok(newest_to_oldest)
}

async fn fetch_recent_server_trade_batches(
    client: ServerClient,
    ticker_info: TickerInfo,
    requested_from: UnixMs,
    to_time: UnixMs,
) -> Result<Vec<Vec<Trade>>, AdapterError> {
    // The generic server API pages forward. Limit its scan to a recent window,
    // then retain the newest bounded tail so a busy symbol cannot strand Renko
    // hours behind live data.
    const RECENT_SERVER_WINDOW_MS: u64 = 15 * 60 * 1_000;
    const MAX_SCAN_PAGES: usize = 8;
    const MAX_RETAINED_TRADES: usize = 80_000;

    let mut cursor = requested_from.max(to_time.saturating_sub(RECENT_SERVER_WINDOW_MS));
    let mut retained = Vec::new();

    for _ in 0..MAX_SCAN_PAGES {
        if cursor >= to_time {
            break;
        }
        let parsed = client
            .fetch_trades_arrow(ticker_info, cursor, to_time, ARROW_LIMIT)
            .await?;
        if parsed.raw_row_count == 0 {
            break;
        }

        cursor = parsed.last_ts.map_or(cursor, |time| time.saturating_add(1));
        retained.extend(parsed.trades);
        if retained.len() > MAX_RETAINED_TRADES {
            let excess = retained.len() - MAX_RETAINED_TRADES;
            retained.drain(..excess);
        }
    }

    retained.sort_by_key(|trade| trade.time);
    Ok((!retained.is_empty())
        .then_some(retained)
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        Ticker, TickerInfo, Timeframe,
        adapter::Exchange,
        unit::{Price, Qty},
    };

    fn kline_stream(exchange: Exchange) -> StreamKind {
        StreamKind::Kline {
            ticker_info: TickerInfo::new(Ticker::new("BTCUSDT", exchange), 0.1, 0.001, None),
            timeframe: Timeframe::M30,
        }
    }

    #[test]
    fn equal_ranges_are_tracked_independently_per_source_stream() {
        let range = FetchRange::Kline(UnixMs::new(1_000), UnixMs::new(2_000));
        let binance = kline_stream(Exchange::BinanceLinear);
        let bybit = kline_stream(Exchange::BybitLinear);
        let mut handler = RequestHandler::default();

        assert!(handler.add_request(range, Some(binance)).unwrap().is_some());
        assert!(matches!(
            handler.add_request(range, Some(binance)),
            Err(ReqError::Overlaps)
        ));
        assert!(handler.add_request(range, Some(bybit)).unwrap().is_some());
    }

    #[test]
    fn explicit_trade_source_wins_over_first_ready_stream() {
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let binance_stream = StreamKind::Trades {
            ticker_info: binance,
        };
        let bybit_stream = StreamKind::Trades { ticker_info: bybit };

        assert_eq!(
            select_trade_stream(Some(bybit_stream), &[binance_stream, bybit_stream]),
            Some(bybit_stream)
        );
    }

    #[test]
    fn timeframe_reset_keeps_completed_footprint_history_requests() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let mut handler = RequestHandler::default();
        let kline = FetchRange::Kline(UnixMs::new(1), UnixMs::new(2));
        let history = FetchRange::FootprintHistoryTrades(UnixMs::new(1), UnixMs::new(2));
        let kline_id = handler
            .add_request(kline, Some(kline_stream(Exchange::BinanceLinear)))
            .unwrap()
            .unwrap();
        let history_id = handler.add_request(history, Some(stream)).unwrap().unwrap();
        handler.mark_completed(kline_id);
        handler.mark_completed(history_id);

        handler.drop_non_footprint_history();

        assert!(
            handler
                .add_request(history, Some(stream))
                .expect("kept footprint request")
                .is_none()
        );
        assert!(
            handler
                .add_request(kline, Some(kline_stream(Exchange::BinanceLinear)))
                .expect("kline request was dropped")
                .is_some()
        );
    }

    #[test]
    fn footprint_history_pages_to_day_end_without_seed_caps() {
        let limits = trade_fetch_limits(true);

        assert_eq!(limits, None);
        assert!(!fetch_limit_reached(limits, usize::MAX, usize::MAX));
    }

    #[test]
    fn failed_footprint_days_retry_with_bounded_attempts() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let range = FetchRange::FootprintHistoryTrades(UnixMs::new(1_000), UnixMs::new(2_000));
        let mut handler = RequestHandler::default();
        let id = handler.add_request(range, Some(stream)).unwrap().unwrap();

        // A failure must not be treated as completed — the day would
        // otherwise silently never populate.
        handler.mark_failed(id);
        assert!(matches!(
            handler.add_request(range, Some(stream)),
            Err(ReqError::Failed)
        ));

        // After the bounded attempt count the range is treated as NoData so
        // a permanently broken source cannot retry forever.
        for _ in 0..RequestHandler::MAX_FETCH_ATTEMPTS {
            handler.mark_failed(id);
        }
        assert!(matches!(
            handler.add_request(range, Some(stream)),
            Err(ReqError::NoData)
        ));
    }

    #[test]
    fn sub_range_of_completed_request_is_skipped_silently() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let mut handler = RequestHandler::default();
        let id = handler
            .add_request(
                FetchRange::Trades(UnixMs::new(1_000), UnixMs::new(10_000)),
                Some(stream),
            )
            .unwrap()
            .unwrap();
        handler.mark_completed(id);

        assert!(matches!(
            handler.add_request(
                FetchRange::Trades(UnixMs::new(4_000), UnixMs::new(5_000)),
                Some(stream),
            ),
            Ok(None)
        ));
    }

    #[test]
    fn completed_footprint_range_allows_targeted_gap_subrange() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let mut handler = RequestHandler::default();
        let outer = FetchRange::FootprintTrades(UnixMs::new(1_000), UnixMs::new(10_000));
        let id = handler.add_request(outer, Some(stream)).unwrap().unwrap();
        handler.mark_completed(id);

        let inner = FetchRange::FootprintTrades(UnixMs::new(4_000), UnixMs::new(5_000));
        assert!(handler.add_request(inner, Some(stream)).unwrap().is_some());
    }

    #[test]
    fn pending_footprint_range_still_blocks_targeted_gap_subrange() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let mut handler = RequestHandler::default();
        handler
            .add_request(
                FetchRange::FootprintTrades(UnixMs::new(1_000), UnixMs::new(10_000)),
                Some(stream),
            )
            .unwrap()
            .unwrap();

        assert!(matches!(
            handler.add_request(
                FetchRange::FootprintTrades(UnixMs::new(4_000), UnixMs::new(5_000)),
                Some(stream),
            ),
            Err(ReqError::Overlaps)
        ));
    }

    #[test]
    fn sub_range_of_pending_request_reports_overlap() {
        let stream = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let mut handler = RequestHandler::default();
        handler
            .add_request(
                FetchRange::Trades(UnixMs::new(1_000), UnixMs::new(10_000)),
                Some(stream),
            )
            .unwrap()
            .unwrap();

        assert!(matches!(
            handler.add_request(
                FetchRange::Trades(UnixMs::new(2_000), UnixMs::new(3_000)),
                Some(stream),
            ),
            Err(ReqError::Overlaps)
        ));
    }

    #[test]
    fn tiny_trade_windows_are_expanded_to_the_minimum_span() {
        let clamped = clamp_trade_window(FetchRange::Trades(
            UnixMs::new(1787295759801),
            UnixMs::new(1787295759820),
        ));

        let span = match clamped {
            FetchRange::Trades(from, to) => to.as_u64() - from.as_u64(),
            other => panic!("unexpected range {other:?}"),
        };
        assert!(span >= MIN_TRADE_FETCH_WINDOW_MS);

        // Non-trade ranges are left untouched.
        let kline = FetchRange::Kline(UnixMs::new(1_000), UnixMs::new(1_010));
        assert_eq!(clamp_trade_window(kline), kline);
    }

    #[test]
    fn containment_respects_stream_and_range_kind() {
        let binance = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let bybit = StreamKind::Trades {
            ticker_info: TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BybitLinear),
                0.1,
                0.001,
                None,
            ),
        };
        let outer = FetchRequest::new(
            FetchRange::Trades(UnixMs::new(1_000), UnixMs::new(10_000)),
            Some(binance),
        );

        // Same stream, contained range of the same kind.
        assert!(outer.contains(&FetchRequest::new(
            FetchRange::Trades(UnixMs::new(2_000), UnixMs::new(3_000)),
            Some(binance),
        )));

        // Different stream is never contained.
        assert!(!outer.contains(&FetchRequest::new(
            FetchRange::Trades(UnixMs::new(2_000), UnixMs::new(3_000)),
            Some(bybit),
        )));

        // Same bounds but a different kind is not contained either.
        assert!(!outer.contains(&FetchRequest::new(
            FetchRange::FootprintTrades(UnixMs::new(2_000), UnixMs::new(3_000)),
            Some(StreamKind::Trades {
                ticker_info: TickerInfo::new(
                    Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                    0.1,
                    0.001,
                    None,
                ),
            }),
        )));
    }

    #[test]
    fn footprint_chart_ranges_are_distinct_from_bounded_generic_seeds() {
        let chart = FetchRange::FootprintTrades(UnixMs::new(1), UnixMs::new(2));
        let generic = FetchRange::Trades(UnixMs::new(1), UnixMs::new(2));

        assert_ne!(chart, generic);
        assert_eq!(trade_fetch_limits(true), None);
    }

    #[test]
    fn generic_trade_seeds_remain_bounded() {
        let limits = trade_fetch_limits(false);

        assert!(fetch_limit_reached(limits, 40, 0));
        assert!(fetch_limit_reached(limits, 0, 80_000));
        assert!(!fetch_limit_reached(limits, 39, 79_999));
    }

    #[test]
    fn kline_requests_without_a_ready_stream_are_reported_undispatched() {
        let stream = kline_stream(Exchange::BinanceLinear);
        let range = FetchRange::Kline(UnixMs::new(1_000), UnixMs::new(2_000));

        // No ready streams: the spec cannot be dispatched and must be handed
        // back so the chart can release it instead of leaving it Pending
        // forever.
        assert!(!fetch_is_dispatchable(&range, None, &[]));

        // With a ready kline stream the same spec dispatches normally.
        assert!(fetch_is_dispatchable(&range, None, &[stream]));

        // An explicit stream always dispatches.
        assert!(fetch_is_dispatchable(&range, Some(stream), &[]));
    }

    #[test]
    fn historical_trade_pages_are_split_into_responsive_ui_chunks() {
        let trade = Trade {
            time: UnixMs::new(1),
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(1.0),
            is_sell: false,
        };
        let chunks = split_trade_batch(vec![trade; TRADE_UI_CHUNK * 2 + 1]);

        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            [10_000, 10_000, 1]
        );
    }

    #[test]
    fn newest_truthful_oi_observation_wins_each_minute() {
        let exchange = [
            OpenInterest {
                time: UnixMs::new(60_000),
                value: 100.0,
            },
            OpenInterest {
                time: UnixMs::new(120_050),
                value: 200.0,
            },
        ];
        let remote = [
            OpenInterest {
                time: UnixMs::new(60_010),
                value: 110.0,
            },
            OpenInterest {
                time: UnixMs::new(120_000),
                value: 190.0,
            },
        ];

        let merged = merge_open_interest_observations(&exchange, &remote);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], remote[0]);
        assert_eq!(merged[1], exchange[1]);
    }

    #[test]
    fn remote_oi_wins_an_exact_timestamp_tie() {
        let exchange = [OpenInterest {
            time: UnixMs::new(60_000),
            value: 100.0,
        }];
        let remote = [OpenInterest {
            time: UnixMs::new(60_000),
            value: 110.0,
        }];

        assert_eq!(merge_open_interest_observations(&exchange, &remote), remote);
    }

    #[test]
    fn exchange_fills_trailing_gap_after_partial_server_prefix() {
        let from = UnixMs::new(1_000);
        let to = UnixMs::new(10_000);
        let first_server = UnixMs::new(1_000);
        let after_last_server = UnixMs::new(6_001);

        assert_eq!(
            exchange_trade_ranges(
                true,
                true,
                from,
                to,
                Some(first_server),
                after_last_server,
                true,
            ),
            vec![(after_last_server, to)]
        );
    }

    #[test]
    fn exchange_fills_both_sides_of_partial_server_history() {
        let from = UnixMs::new(1_000);
        let to = UnixMs::new(10_000);

        assert_eq!(
            exchange_trade_ranges(
                true,
                true,
                from,
                to,
                Some(UnixMs::new(3_000)),
                UnixMs::new(6_001),
                true,
            ),
            vec![(from, UnixMs::new(2_999)), (UnixMs::new(6_001), to),]
        );
    }

    #[test]
    fn server_ranges_before_earliest_history_are_skipped() {
        let earliest = Some(UnixMs::new(1_000));

        assert!(!server_range_may_have_data(UnixMs::new(999), earliest));
        assert!(server_range_may_have_data(UnixMs::new(1_000), earliest));
        assert!(server_range_may_have_data(UnixMs::new(0), None));
    }
}
