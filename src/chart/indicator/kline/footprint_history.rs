use crate::{
    chart::{Caches, Interaction, Message, ViewState, indicator::kline::KlineIndicatorImpl},
    connector::fetcher::{TradeFetchMode, trade_fetch_mode},
    style,
};

use data::{chart::PlotData, chart::kline::KlineDataPoint, util::abbr_large_numbers};
use exchange::{
    OpenInterest, SizeUnit, Ticker, TickerInfo, Trade, UnixMs,
    adapter::{MarketKind, Venue},
    unit::{Price, PriceStep, qty::volume_size_unit},
};
use iced::widget::canvas::{self, Cache, Geometry};
use iced::{
    Alignment, Color, Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, mouse,
    widget::{Canvas, container, responsive, row, rule, scrollable, space},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
const ONE_MIN_MS: u64 = 60 * 1_000;
const FIVE_MIN_MS: u64 = 5 * 60 * 1_000;
pub(crate) const DELTA_ROUNDING_EPSILON: f64 = 0.01;
pub(crate) const DAYS: usize = 3;
/// v2: binary (bincode) encoding. A busy day serializes an order of magnitude
/// faster than the previous JSON format, which blocked the UI thread for
/// seconds per file when loading or persisting day caches.
/// v3: DayStats additionally retains individual large trades for the Large
/// Trades overlay. Older caches cannot be upgraded in place, so they are
/// dropped and re-fetched once.
/// v4: the per-day large-trade cap keeps the largest notionals, not the
/// oldest. v3 caches on busy symbols evicted $1M+ rally prints to make room
/// for $10k tape, so they must be dropped and re-fetched.
/// v5: Binance intraday aggTrades no longer persist truncated busy windows
/// (hour-capped slices + fromId continuation). v4 days fetched during a
/// rally can be missing those timespots and must be re-fetched.
/// v6: Cumulative Delta used to persist the shared day file without large
/// prints, then win the writer race and wipe whales that Daily Delta / live
/// tape had already stored. Those files must be dropped and re-fetched.
/// v7: quote-normalized and inverse trades are no longer multiplied by price
/// a second time when producing USD notionals.
pub(crate) const CACHE_SCHEMA_VERSION: u16 = 7;
const MAX_LOOKBACK_DAYS: usize = 732;
// v2 invalidates any completion proof produced before Hyperliquid replay IDs
// were deduplicated and its legacy recorder coverage was retired. Valid days
// are fetched once from the recorder again; unproven days remain uncached.
const CACHE_PROOF_VERSION: &str = "flowsurface-complete-range-v2";

fn cache_proof_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}.proof",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("cache")
    ))
}

fn cache_bytes_fingerprint(bytes: &[u8]) -> String {
    // FNV-1a is used only to bind the completion proof to exact cache bytes;
    // this is corruption/stale-marker detection, not authentication.
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{CACHE_PROOF_VERSION}\n{}\n{hash:016x}\n", bytes.len())
}

pub(crate) fn cache_has_completion_proof(path: &Path, bytes: &[u8]) -> bool {
    std::fs::read_to_string(cache_proof_path(path))
        .is_ok_and(|proof| proof == cache_bytes_fingerprint(bytes))
}

pub(crate) fn write_cache_with_completion_proof(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let proof_path = cache_proof_path(path);
    let cache_temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let proof_temp = proof_path.with_extension(format!("proof.{}.tmp", uuid::Uuid::new_v4()));

    // Invalidate the old proof before replacing bytes. A crash at any later
    // point leaves an untrusted cache, never a falsely trusted one.
    if proof_path.exists() {
        std::fs::remove_file(&proof_path)?;
    }
    std::fs::write(&cache_temp, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(err) = std::fs::rename(&cache_temp, path) {
        let _ = std::fs::remove_file(&cache_temp);
        return Err(err);
    }

    std::fs::write(&proof_temp, cache_bytes_fingerprint(bytes))?;
    if let Err(err) = std::fs::rename(&proof_temp, &proof_path) {
        let _ = std::fs::remove_file(&proof_temp);
        return Err(err);
    }
    Ok(())
}

/// Notional floor (quote currency) at which an executed trade is retained for
/// the Large Trades overlay. Matches the lowest configurable UI threshold so
/// lowering the threshold never requires a re-backfill within retention.
fn large_trades_capture_floor() -> f64 {
    f64::from(data::chart::kline::Config::LARGE_TRADES_MIN_USD_MIN)
}

pub(crate) fn trade_notional(trade: Trade, qty_is_quote: bool) -> f64 {
    let qty = trade.qty.to_f64();
    if qty_is_quote {
        qty
    } else {
        trade.price.to_f64() * qty
    }
}

/// Hard per-day ceiling so busy symbols cannot grow the shared day book
/// without bound. Retention is by notional, not recency: when the cap is hit
/// the smallest stored prints are dropped so a $10k flood cannot evict the
/// $1M+ trades the overlay is for. The $1_000_000 capture floor on BTCUSDT
/// still fills this cap on the busiest sessions.
const MAX_LARGE_TRADES_PER_DAY: usize = 100_000;
/// Compact only after this many extras have accumulated, so a busy tape is
/// not sorted on every insert.
const LARGE_TRADES_COMPACT_AT: usize = MAX_LARGE_TRADES_PER_DAY * 2;

/// One executed trade retained by the Large Trades overlay. Lives inside the
/// shared per-venue UTC-day book, inheriting its disk persistence, retention
/// and live/historical seam handling.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub(crate) struct StoredLargeTrade {
    pub(crate) time: UnixMs,
    pub(crate) price_units: i64,
    pub(crate) notional: f64,
    pub(crate) is_sell: bool,
}

/// Inclusive UTC-day windows from `00:00:00.000` through `23:59:59.999`,
/// oldest first. The last window is clipped to `cutoff` so today stops at now.
pub(crate) fn utc_day_ranges(cutoff: UnixMs, days: u64) -> Vec<(UnixMs, UnixMs)> {
    let today = day_start(cutoff);
    let start = today.saturating_sub(days.saturating_sub(1) * DAY_MS);
    utc_days_covering(UnixMs::new(start), cutoff, days.max(1) as usize)
}

/// UTC-day windows that overlap `[from, to]`, newest-capped at `cap`, oldest first.
pub(crate) fn utc_days_covering(from: UnixMs, to: UnixMs, cap: usize) -> Vec<(UnixMs, UnixMs)> {
    let to = if to.as_u64() < from.as_u64() {
        from
    } else {
        to
    };
    let first = day_start(from);
    let last = day_start(to);
    let cap = cap.max(1);
    let mut starts = Vec::new();
    let mut start = last;
    loop {
        starts.push(start);
        if start <= first || starts.len() >= cap {
            break;
        }
        let prev = start.saturating_sub(DAY_MS);
        if prev >= start {
            break;
        }
        start = prev;
    }
    starts.reverse();
    starts
        .into_iter()
        .map(|start| {
            let end = start
                .saturating_add(DAY_MS)
                .saturating_sub(1)
                .min(to.as_u64());
            (UnixMs::new(start), UnixMs::new(end))
        })
        .collect()
}

pub(crate) fn missing_day_range(
    start: UnixMs,
    end: UnixMs,
    covered_through: Option<UnixMs>,
) -> Option<(UnixMs, UnixMs)> {
    if covered_through.is_some_and(|through| through >= end) {
        return None;
    }
    let missing_start = covered_through
        .map(|through| through.saturating_add(1))
        .unwrap_or(start)
        .max(start);
    (missing_start <= end).then_some((missing_start, end))
}

fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= previous_end.saturating_add(1)
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub(crate) struct LevelStats {
    bid: f64,
    ask: f64,
}

impl LevelStats {
    pub(crate) fn volume(self) -> f64 {
        self.bid + self.ask
    }

    pub(crate) fn delta(self) -> f64 {
        self.ask - self.bid
    }

    pub(crate) fn add_notional(&mut self, is_sell: bool, notional: f64) {
        if is_sell {
            self.bid += notional;
        } else {
            self.ask += notional;
        }
    }

    fn merge(&mut self, other: Self) {
        self.bid += other.bid;
        self.ask += other.ask;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
struct LargestTrade {
    time: UnixMs,
    is_sell: bool,
    price: f64,
    notional: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub(crate) struct DayStats {
    first: Option<(UnixMs, f64)>,
    last: Option<(UnixMs, f64)>,
    pub(crate) high: Option<f64>,
    pub(crate) low: Option<f64>,
    buy: f64,
    sell: f64,
    notional: f64,
    pub levels: BTreeMap<i64, LevelStats>,
    /// Session-local minute resolution for CVD. This is deliberately omitted
    /// from the shared v7 footprint cache so existing day books remain valid;
    /// cached history stays at its truthful five-minute source resolution.
    #[serde(skip)]
    one_min_delta: BTreeMap<u64, f64>,
    five_min_delta: BTreeMap<u64, f64>,
    largest_trade: Option<LargestTrade>,
    pub(crate) large_trades: Vec<StoredLargeTrade>,
}

/// Which `DayStats` maps a consumer actually reads. Wrappers that only need
/// large prints or five-minute deltas skip the rest so live tape and
/// multi-day backfill do not fill unused BTreeMaps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DayBookRetain {
    pub levels: bool,
    pub one_min_delta: bool,
    pub five_min_delta: bool,
    pub large_trades: bool,
}

/// One active venue's execution-derived CVD inputs for a UTC day.
///
/// `missing_ranges` is the exact recorder-uncovered complement for this
/// source. Keeping it source-local lets aggregate CVD retain the venues that
/// are proven for a bucket instead of discarding every venue when only one
/// recorder stream has a gap.
pub(crate) struct CvdSourceDay {
    pub(crate) deltas: BTreeMap<u64, f64>,
    pub(crate) missing_ranges: Vec<(u64, u64)>,
}

impl DayBookRetain {
    pub const ALL: Self = Self {
        levels: true,
        one_min_delta: true,
        five_min_delta: true,
        large_trades: true,
    };
    /// Large Trades overlay: keep prints, skip price-level and CVD maps.
    pub const LARGE_TRADES_ONLY: Self = Self {
        levels: false,
        one_min_delta: false,
        five_min_delta: false,
        large_trades: true,
    };
    /// CVD subplot: keep minute/five-minute buckets and levels so the shared disk
    /// cache stays complete for Footprint History. Large prints are still
    /// captured on insert so a CVD persist cannot wipe the Large Trades
    /// overlay; they are slimmed from memory after a complete day is written.
    #[cfg(test)]
    pub const DELTAS_WITHOUT_PRINTS: Self = Self {
        levels: true,
        one_min_delta: true,
        five_min_delta: true,
        large_trades: false,
    };
    /// CVD runtime: retain only the exact aggregates it renders. This variant
    /// deliberately cannot write the shared Footprint History cache because
    /// that cache requires price levels and large prints to be authoritative.
    pub const CVD_DELTAS_ONLY: Self = Self {
        levels: false,
        one_min_delta: true,
        five_min_delta: true,
        large_trades: false,
    };
}

impl DayStats {
    #[cfg(test)]
    fn insert_trade(&mut self, trade: Trade) {
        self.insert_trade_with(trade, DayBookRetain::ALL);
    }

    #[cfg(test)]
    fn insert_trade_with(&mut self, trade: Trade, retain: DayBookRetain) {
        self.insert_trade_with_unit(trade, retain, false);
    }

    fn insert_trade_with_unit(&mut self, trade: Trade, retain: DayBookRetain, qty_is_quote: bool) {
        let price = trade.price.to_f64();
        let notional = trade_notional(trade, qty_is_quote);

        if self.first.is_none_or(|(time, _)| trade.time < time) {
            self.first = Some((trade.time, price));
        }
        if self.last.is_none_or(|(time, _)| trade.time > time) {
            self.last = Some((trade.time, price));
        }
        self.high = Some(self.high.map_or(price, |value| value.max(price)));
        self.low = Some(self.low.map_or(price, |value| value.min(price)));
        if trade.is_sell {
            self.sell += notional;
        } else {
            self.buy += notional;
        }
        self.notional += notional;

        if retain.levels {
            let level = self.levels.entry(trade.price.units).or_default();
            if trade.is_sell {
                level.bid += notional;
            } else {
                level.ask += notional;
            }
        }

        let signed_notional = if trade.is_sell { -notional } else { notional };
        if retain.one_min_delta {
            let bucket = trade.time.as_u64() / ONE_MIN_MS * ONE_MIN_MS;
            *self.one_min_delta.entry(bucket).or_default() += signed_notional;
        }
        if retain.five_min_delta {
            let bucket = trade.time.as_u64() / FIVE_MIN_MS * FIVE_MIN_MS;
            *self.five_min_delta.entry(bucket).or_default() += signed_notional;
        }

        if self
            .largest_trade
            .is_none_or(|largest| notional > largest.notional)
        {
            self.largest_trade = Some(LargestTrade {
                time: trade.time,
                is_sell: trade.is_sell,
                price,
                notional,
            });
        }

        // Full day-book writers and the Large Trades consumer retain prints.
        // Compact CVD staging must not build a large-print vector it never
        // renders and is intentionally forbidden from persisting.
        if (retain.large_trades || retain.levels) && notional >= large_trades_capture_floor() {
            self.push_large_trade(StoredLargeTrade {
                time: trade.time,
                price_units: trade.price.units,
                notional,
                is_sell: trade.is_sell,
            });
        }
    }

    fn merge_large_trades_from(&mut self, other: &Self) {
        if other.large_trades.is_empty() {
            return;
        }
        if self.large_trades.is_empty() {
            self.large_trades.clone_from(&other.large_trades);
            return;
        }
        let existing: FxHashSet<(u64, i64, bool, u64)> = self
            .large_trades
            .iter()
            .map(|trade| {
                (
                    trade.time.as_u64(),
                    trade.price_units,
                    trade.is_sell,
                    trade.notional.to_bits(),
                )
            })
            .collect();
        for trade in &other.large_trades {
            let key = (
                trade.time.as_u64(),
                trade.price_units,
                trade.is_sell,
                trade.notional.to_bits(),
            );
            if existing.contains(&key) {
                continue;
            }
            self.push_large_trade(*trade);
        }
        self.compact_large_trades();
    }

    fn push_large_trade(&mut self, trade: StoredLargeTrade) {
        self.large_trades.push(trade);
        if self.large_trades.len() >= LARGE_TRADES_COMPACT_AT {
            self.compact_large_trades();
        }
    }

    /// Keep the largest notionals. FIFO eviction at the capture floor
    /// dropped rally-sized prints on busy majors once a few hours of
    /// threshold-sized tape filled the cap.
    fn compact_large_trades(&mut self) {
        if self.large_trades.len() <= MAX_LARGE_TRADES_PER_DAY {
            return;
        }
        self.large_trades
            .sort_unstable_by(|a, b| b.notional.total_cmp(&a.notional));
        self.large_trades.truncate(MAX_LARGE_TRADES_PER_DAY);
    }

    fn merge(&mut self, other: &Self) {
        if let Some(first) = other.first
            && self.first.is_none_or(|current| first.0 < current.0)
        {
            self.first = Some(first);
        }
        if let Some(last) = other.last
            && self.last.is_none_or(|current| last.0 > current.0)
        {
            self.last = Some(last);
        }
        if let Some(high) = other.high {
            self.high = Some(self.high.map_or(high, |value| value.max(high)));
        }
        if let Some(low) = other.low {
            self.low = Some(self.low.map_or(low, |value| value.min(low)));
        }
        self.buy += other.buy;
        self.sell += other.sell;
        self.notional += other.notional;
        for (price, level) in &other.levels {
            self.levels.entry(*price).or_default().merge(*level);
        }
        for (bucket, delta) in &other.one_min_delta {
            *self.one_min_delta.entry(*bucket).or_default() += delta;
        }
        for (bucket, delta) in &other.five_min_delta {
            *self.five_min_delta.entry(*bucket).or_default() += delta;
        }
        if let Some(largest) = other.largest_trade
            && self
                .largest_trade
                .is_none_or(|current| largest.notional > current.notional)
        {
            self.largest_trade = Some(largest);
        }
        // Large prints are read per-venue by the overlay. Copying them into
        // display merges (Daily Delta, standalone Footprint History) would
        // clone and sort up to 100k entries per venue on every rebuild.
    }

    fn volume(&self) -> f64 {
        self.buy + self.sell
    }

    fn delta(&self) -> f64 {
        self.buy - self.sell
    }

    fn cvd_range(&self) -> (f64, f64) {
        let mut current: f64 = 0.0;
        let mut low: f64 = 0.0;
        let mut high: f64 = 0.0;
        for delta in self.five_min_delta.values() {
            current += delta;
            low = low.min(current);
            high = high.max(current);
        }
        (low, high)
    }

    #[cfg(test)]
    pub(crate) fn five_min_deltas(&self) -> &BTreeMap<u64, f64> {
        &self.five_min_delta
    }

    /// One-minute USD-delta buckets used by CVD on every time-based kline
    /// chart. These preserve exact bucket closes without retaining raw trades.
    pub(crate) fn one_min_deltas(&self) -> &BTreeMap<u64, f64> {
        &self.one_min_delta
    }

    fn one_min_deltas_cover_five_minute(&self) -> bool {
        if self.one_min_delta.is_empty() {
            return self.five_min_delta.is_empty();
        }
        let mut aggregated = BTreeMap::<u64, f64>::new();
        for (minute, delta) in &self.one_min_delta {
            let bucket = minute / FIVE_MIN_MS * FIVE_MIN_MS;
            *aggregated.entry(bucket).or_default() += delta;
        }
        aggregated.len() == self.five_min_delta.len()
            && aggregated.iter().all(|(bucket, delta)| {
                self.five_min_delta
                    .get(bucket)
                    .is_some_and(|five_min| (five_min - delta).abs() <= DELTA_ROUNDING_EPSILON)
            })
    }

    fn largest_bucket(&self, positive: bool) -> Option<(u64, f64)> {
        self.five_min_delta
            .iter()
            .filter(|(_, value)| {
                if positive {
                    **value > 0.0
                } else {
                    **value < 0.0
                }
            })
            .max_by(|left, right| {
                let left = if positive { *left.1 } else { -*left.1 };
                let right = if positive { *right.1 } else { -*right.1 };
                left.total_cmp(&right)
            })
            .map(|(time, value)| (*time, *value))
    }
}

fn source_cvd_deltas_from(stats: &DayStats, from: u64) -> BTreeMap<u64, f64> {
    let mut deltas = stats
        .five_min_delta
        .range(from..)
        .map(|(bucket, delta)| (*bucket, *delta))
        .collect::<BTreeMap<_, _>>();
    for (minute, delta) in stats.one_min_deltas().range(from..) {
        let five_min_bucket = minute / FIVE_MIN_MS * FIVE_MIN_MS;
        if five_min_bucket >= from
            && stats.five_min_delta.contains_key(&five_min_bucket)
            && let Some(coarse) = deltas.get_mut(&five_min_bucket)
        {
            *coarse -= *delta;
        }
        *deltas.entry(*minute).or_default() += *delta;
    }
    deltas.retain(|_, delta| delta.abs() > DELTA_ROUNDING_EPSILON);
    deltas
}

#[derive(Debug, Clone, Copy, Default)]
struct OiDay {
    first: Option<(UnixMs, f64)>,
    last: Option<(UnixMs, f64)>,
}

impl OiDay {
    fn insert(&mut self, value: OpenInterest) {
        if self.first.is_none_or(|(time, _)| value.time < time) {
            self.first = Some((value.time, value.value));
        }
        if self.last.is_none_or(|(time, _)| value.time > time) {
            self.last = Some((value.time, value.value));
        }
    }

    fn delta(self) -> Option<f64> {
        Some(self.last?.1 - self.first?.1)
    }
}

#[derive(Debug, Default)]
struct SourceHistory {
    days: BTreeMap<u64, DayStats>,
    oi: BTreeMap<u64, OiDay>,
}

#[derive(Debug)]
struct HistoricalTradeStage {
    source: Ticker,
    days: BTreeMap<u64, DayStats>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CachedDayStats {
    schema_version: u16,
    /// Canonical `"Exchange:Symbol"` string. Stored as text because `Ticker`'s
    /// backwards-compatible untagged deserializer is unusable with binary
    /// formats like bincode.
    source: String,
    day_start: u64,
    covered_through: u64,
    size_unit: SizeUnit,
    stats: DayStats,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DisplayDay {
    pub stats: DayStats,
    oi_delta: Option<f64>,
}

pub struct FootprintHistoryIndicator {
    cache: Caches,
    sources: Vec<TickerInfo>,
    aggregate: bool,
    lookback_days: usize,
    histories: FxHashMap<Ticker, SourceHistory>,
    history_cutoffs: FxHashMap<Ticker, UnixMs>,
    /// Boundary immediately before the first accepted live execution for each
    /// source. Recorder gaps after this point belong to the live stream and
    /// must not permanently invalidate the bucket that contains the seam.
    live_history_cutoffs: FxHashMap<Ticker, UnixMs>,
    /// Set once the planner requests historical executions. Until then the
    /// indicator intentionally operates in explicit live-only mode.
    history_requested: bool,
    historical_days_started: FxHashSet<(Ticker, u64)>,
    /// Request-owned historical deltas. Pages are not visible and cannot be
    /// persisted until their fetch reaches terminal completion.
    historical_staging: FxHashMap<uuid::Uuid, HistoricalTradeStage>,
    /// Days whose book holds live trades captured after the backfill cutoff.
    /// Their first historical batch merges instead of rebuilding so the live
    /// seam is never destroyed (historical ranges never exceed the cutoff).
    live_seam_days: FxHashSet<(Ticker, u64)>,
    completed_days: FxHashSet<(Ticker, u64)>,
    cache_checkpoints: FxHashMap<(Ticker, u64), Option<UnixMs>>,
    /// Exact recorder holes for requests whose proven segments were retained
    /// in memory. These ranges never count as cache coverage.
    incomplete_ranges: FxHashMap<(Ticker, u64), Vec<(u64, u64)>>,
    retain: DayBookRetain,
    /// When true, keep a merged 3-day display snapshot so iced `view()` does
    /// not clone/merge venue level maps on every mouse move.
    track_display: bool,
    display: Box<[DisplayDay; DAYS]>,
    display_today: u64,
    display_dirty: bool,
    last_display_refresh: Option<Instant>,
}

impl FootprintHistoryIndicator {
    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            sources: Vec::new(),
            aggregate: true,
            lookback_days: DAYS,
            histories: FxHashMap::default(),
            history_cutoffs: FxHashMap::default(),
            live_history_cutoffs: FxHashMap::default(),
            history_requested: false,
            historical_days_started: FxHashSet::default(),
            historical_staging: FxHashMap::default(),
            live_seam_days: FxHashSet::default(),
            completed_days: FxHashSet::default(),
            cache_checkpoints: FxHashMap::default(),
            incomplete_ranges: FxHashMap::default(),
            retain: DayBookRetain::ALL,
            track_display: false,
            display: Box::default(),
            display_today: 0,
            display_dirty: false,
            last_display_refresh: None,
        }
    }

    /// Subplot and standalone Footprint History: cache the merged day books
    /// used by the canvas so panning, hovering, and ticks do not re-merge.
    pub fn new_display() -> Self {
        let mut indicator = Self::new();
        indicator.track_display = true;
        indicator
    }

    pub(crate) fn set_day_book_retain(&mut self, retain: DayBookRetain) {
        self.retain = retain;
    }

    fn rebuild_display(&mut self) {
        if !self.track_display {
            return;
        }
        let now = UnixMs::now();
        self.display_today = day_start(now);
        let days = self.display_day_list(now, DAYS);
        *self.display = std::array::from_fn(|index| days.get(index).cloned().unwrap_or_default());
        self.display_dirty = false;
        self.last_display_refresh = Some(Instant::now());
    }

    pub(crate) fn has_pending_display_refresh(&self) -> bool {
        self.track_display && self.display_dirty
    }

    /// Publish the derived venue-merged display at most ten times per second.
    /// Source day books are updated immediately; only canvas publication is
    /// bounded, so no execution is discarded or approximated.
    pub(crate) fn flush_display_if_due(&mut self, force: bool) -> bool {
        const DISPLAY_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
        if !self.track_display {
            return false;
        }
        if day_start(UnixMs::now()) != self.display_today {
            self.display_dirty = true;
        }
        if !self.display_dirty {
            return false;
        }
        let now = Instant::now();
        let due = force
            || self
                .last_display_refresh
                .is_none_or(|previous| now.duration_since(previous) >= DISPLAY_REFRESH_INTERVAL);
        if !due {
            return false;
        }
        self.rebuild_display();
        self.cache.clear_all();
        true
    }

    pub(crate) fn set_lookback_days(&mut self, days: u16) {
        self.lookback_days = usize::from(days).clamp(1, MAX_LOOKBACK_DAYS);
    }

    fn cache_path(source: TickerInfo, day: u64) -> PathBuf {
        let unit = match volume_size_unit() {
            SizeUnit::Base => "base",
            SizeUnit::Quote => "quote",
        };
        let identity = source.ticker.symbol_and_exchange_string();
        let safe_identity = identity
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let relative = format!(
            "market_data/footprint-days/v{CACHE_SCHEMA_VERSION}/{unit}/{safe_identity}/{day}.fpbin"
        );
        if let Ok(override_path) = std::env::var("FLOWSURFACE_DATA_PATH") {
            let override_path = PathBuf::from(override_path);
            let base = if override_path.is_dir() || override_path.extension().is_none() {
                override_path
            } else {
                override_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            };
            base.join(relative)
        } else {
            data::data_path(Some(&relative))
        }
    }

    fn read_cached_day(path: &Path, source: TickerInfo, day: u64) -> Option<(DayStats, UnixMs)> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                log::warn!("Failed to read footprint day cache {path:?}: {err}");
                return None;
            }
        };
        if !cache_has_completion_proof(path, &bytes) {
            log::warn!(
                "Footprint cache {path:?} predates verified range proofs; preserving it on disk but refusing its checkpoint"
            );
            return None;
        }
        let (cached, _) = match bincode::serde::decode_from_slice::<CachedDayStats, _>(
            &bytes,
            bincode::config::standard(),
        ) {
            Ok(result) => result,
            Err(err) => {
                log::warn!("Ignoring corrupt footprint day cache {path:?}: {err}");
                let _ = std::fs::remove_file(path);
                return None;
            }
        };
        let parsed_source = Ticker::parse_symbol_and_exchange(&cached.source);
        if cached.schema_version != CACHE_SCHEMA_VERSION
            || !parsed_source.is_some_and(|ticker| ticker.same_market(&source.ticker))
            || cached.day_start != day
            || cached.covered_through < day
            || cached.covered_through >= day.saturating_add(DAY_MS)
            || cached.size_unit != volume_size_unit()
        {
            log::warn!("Ignoring mismatched footprint day cache {path:?}");
            let _ = std::fs::remove_file(path);
            return None;
        }
        Some((cached.stats, UnixMs::new(cached.covered_through)))
    }

    fn write_cached_day(
        path: &Path,
        source: TickerInfo,
        day: u64,
        covered_through: UnixMs,
        stats: &DayStats,
    ) -> std::io::Result<()> {
        static WRITE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if stats.notional > 0.0 && stats.levels.is_empty() {
            return Ok(());
        }
        let mut stats = stats.clone();
        if let Some((existing, existing_through)) = Self::read_cached_day(path, source, day) {
            if existing_through > covered_through {
                return Ok(());
            }
            if existing_through == covered_through
                && existing.large_trades.len() >= stats.large_trades.len()
                && existing.levels.len() >= stats.levels.len()
            {
                return Ok(());
            }
            // Extending coverage, or replacing a print-less CVD snapshot:
            // keep whales the writer itself never held in memory.
            stats.merge_large_trades_from(&existing);
        }
        let Some(parent) = path.parent() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "footprint cache path has no parent",
            ));
        };
        std::fs::create_dir_all(parent)?;
        let payload = CachedDayStats {
            schema_version: CACHE_SCHEMA_VERSION,
            source: source.ticker.symbol_and_exchange_string(),
            day_start: day,
            covered_through: covered_through.as_u64(),
            size_unit: volume_size_unit(),
            stats,
        };
        let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard())
            .map_err(std::io::Error::other)?;
        write_cache_with_completion_proof(path, &bytes)
    }

    pub(crate) fn load_day_cache(&mut self, source: TickerInfo, day: UnixMs) -> Option<UnixMs> {
        let day = day_start(day);
        if day < self.retain_oldest(UnixMs::now()) {
            // Another shared-history overlay may need this older day. Treat it
            // as satisfied here without retaining an irrelevant duplicate.
            return Some(UnixMs::new(day.saturating_add(DAY_MS).saturating_sub(1)));
        }
        let path = Self::cache_path(source, day);
        self.load_day_cache_from_path(source, day, &path)
    }

    fn load_day_cache_from_path(
        &mut self,
        source: TickerInfo,
        day: u64,
        path: &Path,
    ) -> Option<UnixMs> {
        if let Some(checkpoint) = self.cache_checkpoints.get(&(source.ticker, day)) {
            return *checkpoint;
        }
        let Some((mut stats, covered_through)) = Self::read_cached_day(path, source, day) else {
            self.cache_checkpoints.insert((source.ticker, day), None);
            return None;
        };
        if !self.retain.levels {
            stats.levels.clear();
        }
        if !self.retain.one_min_delta {
            stats.one_min_delta.clear();
        }
        if !self.retain.five_min_delta {
            stats.five_min_delta.clear();
        }
        if !self.retain.large_trades
            && (!self.retain.levels || Self::day_is_complete(UnixMs::new(day), covered_through))
        {
            stats.large_trades.clear();
        }
        let history = self.histories.entry(source.ticker).or_default();
        if self.live_seam_days.contains(&(source.ticker, day)) {
            // Live prints can arrive before the first cache lookup. The cache
            // covers only through the frozen backfill checkpoint, so merge it
            // underneath the protected post-cutoff live layer instead of
            // replacing the in-memory day.
            let target = history.days.entry(day).or_default();
            target.merge(&stats);
            target.merge_large_trades_from(&stats);
        } else {
            history.days.insert(day, stats);
        }
        self.cache_checkpoints
            .insert((source.ticker, day), Some(covered_through));
        if covered_through.as_u64() >= day.saturating_add(DAY_MS).saturating_sub(1) {
            self.completed_days.insert((source.ticker, day));
        }
        self.historical_days_started.insert((source.ticker, day));
        self.rebuild_display();
        log::debug!("Loaded footprint day cache for {} at {day}", source.ticker);
        Some(covered_through)
    }

    pub(crate) fn persist_day_cache(
        &mut self,
        source: TickerInfo,
        day: UnixMs,
        covered_through: UnixMs,
    ) {
        let day = day_start(day);
        if self
            .cache_checkpoints
            .get(&(source.ticker, day))
            .copied()
            .flatten()
            .is_some_and(|checkpoint| checkpoint >= covered_through)
        {
            return;
        }
        self.accept_verified_day(source, UnixMs::new(day), covered_through);
        let Some(stats) = self
            .histories
            .get_mut(&source.ticker)
            .and_then(|history| history.days.get_mut(&day))
        else {
            return;
        };
        stats.compact_large_trades();
        let complete = Self::day_is_complete(UnixMs::new(day), covered_through);
        if !self.retain.levels {
            // Incomplete books (Large Trades skips price levels) must not
            // occupy the shared on-disk path: Footprint History would treat
            // an empty-level file as a complete day and skip the real fetch.
            // They still consumed this request successfully in memory. Advance
            // their local checkpoint or the planner will request the identical
            // missing suffix again for the rest of the session.
            return;
        }
        let path = Self::cache_path(source, day);
        // Encoding a busy day still costs tens of milliseconds and the write
        // itself blocks on disk — keep both off the UI thread. The clone is
        // the only main-thread cost and is far cheaper than encoding.
        let to_write = stats.clone();
        if !self.retain.large_trades && complete {
            stats.large_trades.clear();
        }
        std::thread::Builder::new()
            .name("footprint-cache-writer".to_string())
            .spawn(move || {
                if let Err(err) =
                    Self::write_cached_day(&path, source, day, covered_through, &to_write)
                {
                    log::warn!("Failed to write footprint day cache {path:?}: {err}");
                }
            })
            .map(|_| ())
            .unwrap_or_else(|err| {
                log::warn!("Failed to spawn footprint cache writer: {err}");
            });
        log::debug!("Stored footprint day cache for {} at {day}", source.ticker);
    }

    pub(crate) fn accept_verified_day(
        &mut self,
        source: TickerInfo,
        day: UnixMs,
        covered_through: UnixMs,
    ) {
        let day = day_start(day);
        self.cache_checkpoints
            .insert((source.ticker, day), Some(covered_through));
        if let Some(ranges) = self.incomplete_ranges.get_mut(&(source.ticker, day)) {
            let through = covered_through.as_u64();
            ranges.retain_mut(|(start, end)| {
                if *end <= through {
                    return false;
                }
                if *start <= through {
                    *start = through.saturating_add(1);
                }
                true
            });
            if ranges.is_empty() {
                self.incomplete_ranges.remove(&(source.ticker, day));
            }
        }
        if Self::day_is_complete(UnixMs::new(day), covered_through) {
            self.completed_days.insert((source.ticker, day));
        }
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn retain_oldest(&self, now: UnixMs) -> u64 {
        day_start(now).saturating_sub((self.lookback_days.saturating_sub(1) as u64) * DAY_MS)
    }

    /// Drop detail CVD does not read from one UTC day across every source.
    /// Minute-complete days keep only minute deltas; legacy cached days retain
    /// their truthful five-minute deltas because they cannot be subdivided.
    /// In-memory only: disk caches are written before this runs and stay
    /// complete for other consumers.
    pub(crate) fn slim_day_for_cvd(&mut self, day: u64) {
        let day = day_start(UnixMs::new(day));
        for history in self.histories.values_mut() {
            if let Some(stats) = history.days.get_mut(&day) {
                stats.levels.clear();
                if stats.one_min_deltas_cover_five_minute() {
                    stats.five_min_delta.clear();
                }
            }
        }
    }

    pub(crate) fn day_is_complete(day: UnixMs, covered_through: UnixMs) -> bool {
        let day = day_start(day);
        covered_through.as_u64() >= day.saturating_add(DAY_MS).saturating_sub(1)
    }

    fn active_sources(&self) -> &[TickerInfo] {
        if self.aggregate {
            &self.sources
        } else {
            self.sources.get(..1).unwrap_or(&[])
        }
    }

    fn day_has_verified_coverage(&self, day: u64) -> bool {
        !self.history_requested
            || self.active_sources().iter().all(|source| {
                self.incomplete_ranges
                    .get(&(source.ticker, day))
                    .is_none_or(Vec::is_empty)
                    && self
                        .cache_checkpoints
                        .get(&(source.ticker, day))
                        .copied()
                        .flatten()
                        .is_some()
            })
    }

    fn cvd_source_missing_ranges(&self, source: TickerInfo, day: u64) -> Vec<(u64, u64)> {
        if !self.history_requested {
            return Vec::new();
        }
        let day = day_start(UnixMs::new(day));
        if let Some(ranges) = self.incomplete_ranges.get(&(source.ticker, day))
            && !ranges.is_empty()
        {
            return ranges.clone();
        }
        if self
            .cache_checkpoints
            .get(&(source.ticker, day))
            .copied()
            .flatten()
            .is_some()
        {
            return Vec::new();
        }
        vec![(day, day.saturating_add(DAY_MS).saturating_sub(1))]
    }

    #[cfg(test)]
    pub(crate) fn cvd_missing_ranges(&self, day: u64) -> Vec<(u64, u64)> {
        if !self.history_requested {
            return Vec::new();
        }
        let day = day_start(UnixMs::new(day));
        let mut missing = Vec::new();
        for source in self.active_sources() {
            missing.extend(self.cvd_source_missing_ranges(*source, day));
        }
        merge_ranges(missing)
    }

    pub(crate) fn cvd_source_days_from(&self, day: u64, from: u64) -> Vec<CvdSourceDay> {
        let day = day_start(UnixMs::new(day));
        self.active_sources()
            .iter()
            .map(|source| {
                let deltas = self
                    .histories
                    .get(&source.ticker)
                    .and_then(|history| history.days.get(&day))
                    .map_or_else(BTreeMap::new, |stats| source_cvd_deltas_from(stats, from));
                CvdSourceDay {
                    deltas,
                    missing_ranges: self.cvd_source_missing_ranges(*source, day),
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn display_days(&self, now: UnixMs) -> [DisplayDay; DAYS] {
        let days = self.display_day_list(now, DAYS);
        std::array::from_fn(|index| days.get(index).cloned().unwrap_or_default())
    }

    pub(crate) fn display_day_at(&self, day: u64) -> DisplayDay {
        let mut result = DisplayDay::default();
        if !self.day_has_verified_coverage(day) {
            return result;
        }
        let mut oi_delta = 0.0;
        let mut has_oi = false;
        for source in self.active_sources() {
            if let Some(history) = self.histories.get(&source.ticker) {
                if let Some(stats) = history.days.get(&day) {
                    result.stats.merge(stats);
                }
                if let Some(delta) = history.oi.get(&day).and_then(|oi| oi.delta()) {
                    oi_delta += delta;
                    has_oi = true;
                }
            }
        }
        result.oi_delta = has_oi.then_some(oi_delta);
        result
    }

    /// Venue-merged CVD deltas for one UTC day, starting at `from`.
    ///
    /// Existing disk caches contribute honest five-minute samples. Trades
    /// ingested this session contribute one-minute samples; their amounts are
    /// subtracted from the matching five-minute total before being reinserted
    /// at minute resolution, so no execution can be counted twice.
    #[cfg(test)]
    pub(crate) fn merged_cvd_deltas_from(&self, day: u64, from: u64) -> BTreeMap<u64, f64> {
        if self.history_requested
            && self.active_sources().iter().any(|source| {
                self.cache_checkpoints
                    .get(&(source.ticker, day))
                    .copied()
                    .flatten()
                    .is_none()
                    && self
                        .incomplete_ranges
                        .get(&(source.ticker, day))
                        .is_none_or(Vec::is_empty)
            })
        {
            // Until every venue has either a verified checkpoint or exact
            // missing intervals, there is no honest boundary at which an
            // aggregate CVD segment can start.
            return BTreeMap::new();
        }
        let mut merged = BTreeMap::<u64, f64>::new();
        for source in self.active_sources() {
            let Some(stats) = self
                .histories
                .get(&source.ticker)
                .and_then(|history| history.days.get(&day))
            else {
                continue;
            };
            for (bucket, delta) in source_cvd_deltas_from(stats, from) {
                *merged.entry(bucket).or_default() += delta;
            }
        }
        merged.retain(|_, delta| delta.abs() > DELTA_ROUNDING_EPSILON);
        merged
    }

    /// Add execution-derived minute deltas for one source/day. Once an exact
    /// minute cache is present, its covered tape supersedes the coarse v7
    /// five-minute fallback for CVD; keeping both would count it twice.
    pub(crate) fn merge_exact_cvd_minutes(
        &mut self,
        source: Ticker,
        day: u64,
        deltas: &BTreeMap<u64, f64>,
    ) {
        let source = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source))
            .map_or(source, |candidate| candidate.ticker);
        let stats = self
            .histories
            .entry(source)
            .or_default()
            .days
            .entry(day_start(UnixMs::new(day)))
            .or_default();
        for (minute, delta) in deltas {
            *stats.one_min_delta.entry(*minute).or_default() += delta;
        }
        stats.five_min_delta.clear();
        self.clear_all_caches();
    }

    /// Visit large executed trades for a UTC day across active sources.
    /// Does not allocate; the overlay keeps only the markers it will draw.
    pub(crate) fn for_each_large_trade(
        &self,
        day_start_ts: u64,
        threshold_usd: f32,
        mut visit: impl FnMut(StoredLargeTrade),
    ) {
        // Individual markers remain truthful even when another interval in the
        // same day is missing. Unlike daily totals or profiles, they do not
        // imply continuous coverage; partial fetches contain only
        // recorder-proven segments.
        let floor =
            f64::from(threshold_usd.max(data::chart::kline::Config::LARGE_TRADES_MIN_USD_MIN));
        for source in self.active_sources() {
            if let Some(stats) = self
                .histories
                .get(&source.ticker)
                .and_then(|history| history.days.get(&day_start_ts))
            {
                for trade in &stats.large_trades {
                    if trade.notional >= floor {
                        visit(*trade);
                    }
                }
            }
        }
    }

    /// Large executed trades recorded for a UTC day, merged across active
    /// sources and pre-filtered by the caller's threshold. Copies only the
    /// matching entries, never the whole day book.
    #[cfg(test)]
    pub(crate) fn display_large_trades(
        &self,
        day_start_ts: u64,
        threshold_usd: f32,
    ) -> Vec<StoredLargeTrade> {
        let mut trades: Vec<StoredLargeTrade> = Vec::new();
        self.for_each_large_trade(day_start_ts, threshold_usd, |trade| trades.push(trade));
        if trades.len() > 1 {
            trades.sort_by_key(|trade| trade.time.as_u64());
        }
        trades
    }

    /// Merge only venues whose entire requested UTC period is available.
    ///
    /// An aggregate should not disappear while an additional venue is still
    /// backfilling, but it must also never mix a partial venue into the value
    /// area. Venues join the merged profile atomically once all of their days
    /// are complete.
    #[cfg(test)]
    pub(crate) fn display_complete_period(&self, start: UnixMs, end: UnixMs) -> (DayStats, usize) {
        let mut result = DayStats::default();
        let mut complete_sources = 0;
        for source in self.active_sources() {
            if !self.source_period_complete(source.ticker, start, end) {
                continue;
            }
            complete_sources += 1;
            if let Some(history) = self.histories.get(&source.ticker) {
                for (_, stats) in history.days.range(start.as_u64()..=day_start(end)) {
                    result.merge(stats);
                }
            }
        }
        (result, complete_sources)
    }

    #[cfg(test)]
    fn source_period_complete(&self, source: Ticker, start: UnixMs, end: UnixMs) -> bool {
        let mut day = day_start(start);
        let last = day_start(end);
        loop {
            if !self.completed_days.contains(&(source, day)) {
                return false;
            }
            if day >= last {
                return true;
            }
            let next = day.saturating_add(DAY_MS);
            if next <= day {
                return false;
            }
            day = next;
        }
    }

    #[cfg(test)]
    pub(crate) fn period_coverage(&self, start: UnixMs, end: UnixMs) -> (usize, usize) {
        let first = day_start(start);
        let last = day_start(end);
        let mut complete = 0;
        let mut expected = 0;
        for source in self.active_sources() {
            let mut day = first;
            loop {
                expected += 1;
                if self.completed_days.contains(&(source.ticker, day)) {
                    complete += 1;
                }
                if day >= last {
                    break;
                }
                let next = day.saturating_add(DAY_MS);
                if next <= day {
                    break;
                }
                day = next;
            }
        }
        (complete, expected)
    }

    pub(crate) fn display_day_list(&self, now: UnixMs, days: usize) -> Vec<DisplayDay> {
        let today = day_start(now);
        let count = days.max(1);
        (0..count)
            .map(|index| {
                let day = today.saturating_sub(index as u64 * DAY_MS);
                self.display_day_at(day)
            })
            .collect()
    }

    fn source_label(&self) -> String {
        let mut names = Vec::new();
        for source in self.active_sources() {
            let venue = source.exchange().venue().to_string();
            if !names.contains(&venue) {
                names.push(venue);
            }
        }
        if self.aggregate && names.len() > 1 {
            format!("Aggregated · {}", names.join(" + "))
        } else {
            names
                .first()
                .cloned()
                .unwrap_or_else(|| "No venue".to_string())
        }
    }

    fn history_note(&self) -> Option<&'static str> {
        let today = day_start(UnixMs::now());
        if self.history_requested && !self.day_has_verified_coverage(today) {
            return Some("History incomplete · waiting for verified venue coverage");
        }
        match trade_fetch_mode() {
            TradeFetchMode::Off => Some("Live data only · enable historical trades in Network"),
            TradeFetchMode::Exchange
                if self
                    .active_sources()
                    .iter()
                    .any(|source| source.exchange().venue() == Venue::Hyperliquid) =>
            {
                Some("Hyperliquid: live data only in Exchange mode · Server enables backfill")
            }
            TradeFetchMode::Server
                if self
                    .active_sources()
                    .iter()
                    .any(|source| source.exchange().venue() == Venue::Hyperliquid) =>
            {
                Some("OI Δ uses Binance / Bybit · Hyperliquid OI is unavailable")
            }
            TradeFetchMode::Exchange | TradeFetchMode::Server => None,
        }
    }

    pub(crate) fn standalone_element(&self, block_step: PriceStep) -> Element<'_, Message> {
        let sources = self.source_label();
        let history_note = self.history_note();
        let min_tick = self
            .active_sources()
            .iter()
            .map(|source| PriceStep::from(source.min_ticksize))
            .min_by_key(|step| step.units)
            .unwrap_or(block_step);
        let row_count = self
            .display
            .iter()
            .map(|day| price_row_count(day, min_tick, block_step))
            .max()
            .unwrap_or_default();

        responsive(move |viewport| {
            let content_height = standalone_content_height(row_count).max(viewport.height);
            let canvas = Canvas::new(FootprintHistoryCanvas {
                cache: &self.cache.main,
                days: &self.display,
                sources: sources.clone(),
                history_note,
                min_tick,
                block_step: Some(block_step),
            })
            .height(content_height)
            .width(Length::Fill);

            scrollable::Scrollable::with_direction(
                canvas,
                scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(8).scroller_width(6),
                ),
            )
            .height(Length::Fill)
            .width(Length::Fill)
            .style(style::scroll_bar)
            .into()
        })
        .into()
    }

    fn clip_incomplete_ranges_to_live_cutoff(&mut self, source: Ticker, cutoff: UnixMs) {
        let cutoff = cutoff.as_u64();
        self.incomplete_ranges.retain(|(candidate, _), ranges| {
            if !candidate.same_market(&source) {
                return true;
            }
            ranges.retain_mut(|(start, end)| {
                if *start > cutoff {
                    return false;
                }
                *end = (*end).min(cutoff);
                true
            });
            !ranges.is_empty()
        });
    }
}

impl KlineIndicatorImpl for FootprintHistoryIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
        if self.track_display && day_start(UnixMs::now()) != self.display_today {
            self.rebuild_display();
        }
    }

    fn clear_crosshair_caches(&mut self) {}

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        _data_labels_always_visible: bool,
        _visible_range: std::ops::RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        let canvas = Canvas::new(FootprintHistoryCanvas {
            cache: &self.cache.main,
            days: &self.display,
            sources: self.source_label(),
            history_note: self.history_note(),
            min_tick: self
                .active_sources()
                .iter()
                .map(|source| PriceStep::from(source.min_ticksize))
                .min_by_key(|step| step.units)
                .unwrap_or(chart.tick_size),
            block_step: None,
        })
        .height(Length::Fill)
        .width(Length::Fill);

        row![
            canvas,
            rule::vertical(1).style(style::split_ruler),
            container(space::vertical()).width(chart.y_labels_width())
        ]
        .into()
    }

    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn set_trade_history_lookback(&mut self, days: u16) {
        self.set_lookback_days(days);
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn reset_trade_history_backfill(&mut self) {
        self.historical_days_started.clear();
        self.historical_staging.clear();
        self.live_seam_days.clear();
        self.completed_days.clear();
        self.cache_checkpoints.clear();
        self.incomplete_ranges.clear();
        self.histories.clear();
        self.history_cutoffs.clear();
        self.live_history_cutoffs.clear();
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn configure_footprint_history(&mut self, sources: &[TickerInfo], aggregate: bool) {
        self.sources = sources.to_vec();
        self.aggregate = aggregate;
        // Histories are keyed by venue ticker and intentionally survive source
        // toggles. Display aggregation is then an inexpensive merge of caches.
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn prepare_footprint_history(&mut self, source: TickerInfo, cutoff: UnixMs) {
        self.history_requested = true;
        // The first planner pass can run before the trade WebSocket connects,
        // making this a provisional boundary. Once the first live execution
        // is known, the planner advances `cutoff` to immediately before it so
        // history can fill the startup seam without overlapping live data.
        self.history_cutoffs
            .entry(source.ticker)
            .and_modify(|current| *current = (*current).max(cutoff))
            .or_insert(cutoff);
    }

    fn load_cached_footprint_day(
        &mut self,
        source: TickerInfo,
        day_start: UnixMs,
    ) -> Option<UnixMs> {
        self.load_day_cache(source, day_start)
    }

    fn persist_cached_footprint_day(
        &mut self,
        source: TickerInfo,
        day_start: UnixMs,
        covered_through: UnixMs,
    ) {
        self.persist_day_cache(source, day_start, covered_through);
    }

    fn on_source_trades(&mut self, source: TickerInfo, trades: &[Trade], historical: bool) {
        let oldest = self.retain_oldest(UnixMs::now());
        if historical
            && trades
                .iter()
                .map(|trade| trade.time.as_u64())
                .max()
                .is_some_and(|latest| latest < oldest)
        {
            // Shared backfills can extend much farther than this indicator's
            // lookback (for example Previous Value Area alongside Daily Delta).
            // Do not scan or invalidate caches for an entirely irrelevant page.
            return;
        }

        let source_key = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map_or(source.ticker, |candidate| candidate.ticker);

        if !historical && let Some(first_live) = trades.iter().map(|trade| trade.time).min() {
            // Stop history immediately before the first accepted live print.
            // This protects pre-hydration live data without replay overlap.
            let live_cutoff = first_live.saturating_sub(1);
            self.history_cutoffs
                .entry(source_key)
                .or_insert(live_cutoff);
            let live_cutoff = *self
                .live_history_cutoffs
                .entry(source_key)
                .and_modify(|current| *current = (*current).min(live_cutoff))
                .or_insert(live_cutoff);
            self.clip_incomplete_ranges_to_live_cutoff(source_key, live_cutoff);
        }
        let cutoff = self.history_cutoffs.get(&source_key).copied();

        if historical {
            let touched_days = trades
                .iter()
                .map(|trade| day_start(trade.time))
                .collect::<FxHashSet<_>>();
            let history = self.histories.entry(source_key).or_default();
            for day in touched_days {
                if self.historical_days_started.insert((source_key, day)) {
                    let overlaps_live_seam = cutoff.is_some_and(|boundary| {
                        trades
                            .iter()
                            .any(|trade| day_start(trade.time) == day && trade.time > boundary)
                    });
                    // Authoritative history replaces whatever the day held,
                    // unless the book already captured live trades from after
                    // the backfill cutoff. Historical fetch ranges are clipped
                    // at that cutoff, so those live captures cannot duplicate
                    // the batch and must survive it: backfill is paced one UTC
                    // day at a time over the whole lookback, so today's batch
                    // can arrive hours into a session and wiping here would
                    // silently delete every large print seen in between.
                    if !self.live_seam_days.remove(&(source_key, day)) || overlaps_live_seam {
                        history.days.remove(&day);
                    }
                }
            }
        }
        let retain = self.retain;
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || source.market_type() == MarketKind::InversePerps;
        {
            let history = self.histories.entry(source_key).or_default();
            for trade in trades.iter() {
                if trade.time.as_u64() < oldest
                    || (!historical && cutoff.is_some_and(|boundary| trade.time <= boundary))
                {
                    continue;
                }
                if !historical && cutoff.is_some_and(|boundary| trade.time > boundary) {
                    // Post-cutoff live capture; protect it from later rebuilds of
                    // its day (see above).
                    self.live_seam_days
                        .insert((source_key, day_start(trade.time)));
                }
                history
                    .days
                    .entry(day_start(trade.time))
                    .or_default()
                    .insert_trade_with_unit(*trade, retain, qty_is_quote);
            }
            history.days.retain(|day, _| *day >= oldest);
        }
        if self.track_display {
            self.display_dirty = true;
            self.flush_display_if_due(false);
        } else {
            self.clear_all_caches();
        }
    }

    fn stage_source_trades(&mut self, req_id: uuid::Uuid, source: TickerInfo, trades: &[Trade]) {
        let oldest = self.retain_oldest(UnixMs::now());
        let source_key = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map_or(source.ticker, |candidate| candidate.ticker);
        let retain = self.retain;
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || source.market_type() == MarketKind::InversePerps;
        let cutoff = self.history_cutoffs.get(&source_key).copied();
        let stage = self
            .historical_staging
            .entry(req_id)
            .or_insert_with(|| HistoricalTradeStage {
                source: source_key,
                days: BTreeMap::new(),
            });
        if !stage.source.same_market(&source_key) {
            // A request id has exactly one source owner. Ignore a mismatched
            // late page instead of allowing it to contaminate another venue.
            return;
        }
        for trade in trades {
            let day = day_start(trade.time);
            let already_covered = self
                .cache_checkpoints
                .get(&(source_key, day))
                .copied()
                .flatten()
                .is_some_and(|checkpoint| trade.time <= checkpoint);
            if trade.time.as_u64() < oldest
                || cutoff.is_some_and(|boundary| trade.time > boundary)
                || already_covered
            {
                continue;
            }
            stage
                .days
                .entry(day)
                .or_default()
                .insert_trade_with_unit(*trade, retain, qty_is_quote);
        }
    }

    fn commit_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        let Some(stage) = self.historical_staging.remove(&req_id) else {
            return;
        };
        let oldest = self.retain_oldest(UnixMs::now());
        let history = self.histories.entry(stage.source).or_default();
        for (day, stats) in stage.days {
            if day < oldest {
                continue;
            }
            let target = history.days.entry(day).or_default();
            target.merge(&stats);
            target.merge_large_trades_from(&stats);
            self.historical_days_started.insert((stage.source, day));
        }
        history.days.retain(|day, _| *day >= oldest);
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn mark_incomplete_trade_history(
        &mut self,
        source: TickerInfo,
        missing_ranges: &[(UnixMs, UnixMs)],
    ) {
        let source = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map_or(source.ticker, |candidate| candidate.ticker);
        let live_cutoff = self.live_history_cutoffs.get(&source).copied();
        for &(start, end) in missing_ranges {
            let mut cursor = start.as_u64();
            let end = live_cutoff.map_or(end.as_u64(), |cutoff| end.as_u64().min(cutoff.as_u64()));
            if cursor > end {
                continue;
            }
            while cursor <= end {
                let day = day_start(UnixMs::new(cursor));
                let clipped_end = end.min(day.saturating_add(DAY_MS).saturating_sub(1));
                self.incomplete_ranges
                    .entry((source, day))
                    .or_default()
                    .push((cursor, clipped_end));
                if clipped_end == u64::MAX {
                    break;
                }
                cursor = clipped_end.saturating_add(1);
            }
        }
        for ranges in self.incomplete_ranges.values_mut() {
            *ranges = merge_ranges(std::mem::take(ranges));
        }
        self.rebuild_display();
        self.clear_all_caches();
    }

    fn discard_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.historical_staging.remove(&req_id);
    }

    fn on_source_open_interest(&mut self, source: TickerInfo, values: &[OpenInterest]) {
        let Some(source_key) = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map(|candidate| candidate.ticker)
        else {
            return;
        };
        let oldest = self.retain_oldest(UnixMs::now());
        {
            let history = self.histories.entry(source_key).or_default();
            for value in values {
                if value.time.as_u64() >= oldest {
                    history
                        .oi
                        .entry(day_start(value.time))
                        .or_default()
                        .insert(*value);
                }
            }
            history.oi.retain(|day, _| *day >= oldest);
        }
        self.rebuild_display();
        self.clear_all_caches();
    }
}

struct FootprintHistoryCanvas<'a> {
    cache: &'a Cache,
    days: &'a [DisplayDay; DAYS],
    sources: String,
    history_note: Option<&'static str>,
    min_tick: PriceStep,
    block_step: Option<PriceStep>,
}

impl canvas::Program<Message> for FootprintHistoryCanvas<'_> {
    type State = Interaction;

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            if bounds.width < 30.0 || bounds.height < 30.0 {
                return;
            }
            let width = bounds.width / DAYS as f32;
            let fg = palette.background.base.text;
            let muted = palette.background.strong.text.scale_alpha(0.72);
            let border = palette.background.strong.color;
            let buy = palette.success.base.color;
            let sell = palette.danger.base.color;
            let accent = palette.primary.base.color;
            let warning = palette.warning.base.color;
            let today = day_start(UnixMs::now());

            draw_text(
                frame,
                &self.sources,
                Point::new(6.0, 10.0),
                12.0,
                muted,
                Alignment::Start,
            );
            if let Some(note) = self.history_note {
                draw_text(
                    frame,
                    note,
                    Point::new(bounds.width - 6.0, 10.0),
                    11.0,
                    warning,
                    Alignment::End,
                );
            }

            let top = 26.0;
            let metrics_height = 156.0_f32.min((bounds.height * 0.34).max(118.0));
            let row_h = ((metrics_height - 28.0) / 9.0).clamp(11.0, 16.0);
            let table_top = top + metrics_height;

            for index in 0..DAYS {
                let left = width * index as f32;
                let right = left + width;
                if index > 0 {
                    frame.fill_rectangle(
                        Point::new(left, top),
                        Size::new(1.0, bounds.height - top),
                        border,
                    );
                }
                let day_start = today.saturating_sub(index as u64 * DAY_MS);
                let title = day_title(index);
                draw_text(
                    frame,
                    title,
                    Point::new(left + 7.0, top + 8.0),
                    13.0,
                    fg,
                    Alignment::Start,
                );
                draw_text(
                    frame,
                    &format_day(day_start),
                    Point::new(right - 7.0, top + 8.0),
                    11.0,
                    muted,
                    Alignment::End,
                );
                draw_day_metrics(
                    frame,
                    &self.days[index],
                    left + 7.0,
                    right - 7.0,
                    top + 24.0,
                    row_h,
                    fg,
                    muted,
                    buy,
                    sell,
                    accent,
                );
            }

            frame.fill_rectangle(
                Point::new(0.0, table_top),
                Size::new(bounds.width, 1.0),
                border,
            );
            let available_rows =
                ((bounds.height - table_top - 36.0) / 15.0).floor().max(1.0) as usize;
            let price_rows = self
                .days
                .iter()
                .map(|day| day_price_rows(day, self.min_tick, self.block_step, available_rows))
                .collect::<Vec<_>>();

            for (index, (step, _)) in price_rows.iter().enumerate() {
                let left = width * index as f32;
                draw_text(
                    frame,
                    &format!("Daily footprint · USD @ {}", format_price_step(*step)),
                    Point::new(left + 7.0, table_top + 12.0),
                    11.0,
                    fg,
                    Alignment::Start,
                );
                draw_table_header(frame, left, width, table_top + 29.0, muted);
            }

            let grouped = self
                .days
                .iter()
                .zip(price_rows.iter())
                .map(|(day, (step, _))| group_levels(&day.stats, *step))
                .collect::<Vec<_>>();
            // Block currently being traded: the row holding each day's most
            // recent traded price, so the live pocket stays easy to spot.
            let traded_blocks = self
                .days
                .iter()
                .zip(price_rows.iter())
                .map(|(day, (step, rows))| {
                    let block = day.stats.last.map(|(_, price)| {
                        Price::from_f64(price).units.div_euclid(step.units) * step.units
                    });
                    block.filter(|block| rows.contains(block))
                })
                .collect::<Vec<_>>();
            let maxima = grouped
                .iter()
                .map(|levels| {
                    let max_side = levels
                        .values()
                        .map(|level| level.bid.max(level.ask))
                        .fold(0.0_f64, f64::max);
                    let max_volume = levels
                        .values()
                        .map(|level| level.volume())
                        .fold(0.0_f64, f64::max);
                    (max_side, max_volume)
                })
                .collect::<Vec<_>>();

            for row_index in 0..available_rows {
                let y = table_top + 38.0 + row_index as f32 * 15.0;
                for day_index in 0..DAYS {
                    let Some(price_units) = price_rows[day_index].1.get(row_index) else {
                        continue;
                    };
                    let left = width * day_index as f32;
                    draw_level_row(
                        frame,
                        left,
                        width,
                        y,
                        *price_units,
                        grouped[day_index].get(price_units).copied(),
                        maxima[day_index],
                        Some(traded_blocks[day_index] == Some(*price_units)),
                        fg,
                        muted,
                        buy,
                        sell,
                        warning,
                        accent,
                    );
                }
            }
        });
        vec![geometry]
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_day_metrics(
    frame: &mut canvas::Frame,
    day: &DisplayDay,
    left: f32,
    right: f32,
    top: f32,
    row_h: f32,
    fg: Color,
    muted: Color,
    buy: Color,
    sell: Color,
    accent: Color,
) {
    let stats = &day.stats;
    let mid = (left + right) * 0.5;
    let value_x = mid - 4.0;
    let right_value_x = right;
    let price = |value: Option<f64>| value.map_or_else(|| "—".to_string(), format_price);
    let metric = |frame: &mut canvas::Frame, label: &str, value: String, y: f32, color: Color| {
        draw_text(
            frame,
            label,
            Point::new(left, y),
            10.0,
            muted,
            Alignment::Start,
        );
        draw_text(
            frame,
            &value,
            Point::new(value_x, y),
            10.0,
            color,
            Alignment::End,
        );
    };

    metric(frame, "Open", price(stats.first.map(|(_, p)| p)), top, fg);
    draw_text(
        frame,
        "High",
        Point::new(mid + 5.0, top),
        10.0,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &price(stats.high),
        Point::new(right_value_x, top),
        10.0,
        fg,
        Alignment::End,
    );
    metric(frame, "Low", price(stats.low), top + row_h, fg);
    draw_text(
        frame,
        "Close",
        Point::new(mid + 5.0, top + row_h),
        10.0,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &price(stats.last.map(|(_, p)| p)),
        Point::new(right_value_x, top + row_h),
        10.0,
        fg,
        Alignment::End,
    );

    metric(
        frame,
        "Volume USD",
        usd(stats.volume()),
        top + row_h * 2.0,
        fg,
    );
    draw_text(
        frame,
        "Notional",
        Point::new(mid + 5.0, top + row_h * 2.0),
        10.0,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &usd(stats.notional),
        Point::new(right_value_x, top + row_h * 2.0),
        10.0,
        fg,
        Alignment::End,
    );

    let delta = stats.delta();
    let delta_pct = if stats.volume() > 0.0 {
        delta / stats.volume() * 100.0
    } else {
        0.0
    };
    metric(
        frame,
        "Delta",
        format!("{} ({delta_pct:+.1}%)", signed_usd(delta)),
        top + row_h * 3.0,
        if delta >= 0.0 { buy } else { sell },
    );

    let (cvd_low, cvd_high) = stats.cvd_range();
    metric(
        frame,
        "CVD range",
        format!("L {}", signed_usd(cvd_low)),
        top + row_h * 4.0,
        sell,
    );
    draw_text(
        frame,
        &format!("H {}", signed_usd(cvd_high)),
        Point::new(right_value_x, top + row_h * 4.0),
        10.0,
        buy,
        Alignment::End,
    );
    if cvd_high > cvd_low {
        let line_left = left + 64.0;
        let line_right = right - 54.0;
        let y = top + row_h * 4.0;
        let ratio = ((delta - cvd_low) / (cvd_high - cvd_low)).clamp(0.0, 1.0) as f32;
        frame.fill_rectangle(
            Point::new(line_left + (line_right - line_left) * ratio - 2.0, y - 2.0),
            Size::new(4.0, 4.0),
            accent,
        );
    }

    metric(
        frame,
        "OI Δ",
        day.oi_delta.map_or_else(|| "n/a".to_string(), signed),
        top + row_h * 5.0,
        day.oi_delta
            .map_or(muted, |value| if value >= 0.0 { buy } else { sell }),
    );

    for (offset, positive, label) in [
        (6.0, true, "Largest +Δ (5m)"),
        (7.0, false, "Largest -Δ (5m)"),
    ] {
        let result = stats.largest_bucket(positive);
        metric(
            frame,
            label,
            result.map_or_else(|| "—".to_string(), |(_, value)| signed_usd(value)),
            top + row_h * offset,
            if positive { buy } else { sell },
        );
        if let Some((time, _)) = result {
            draw_text(
                frame,
                &format_time(time),
                Point::new(right_value_x, top + row_h * offset),
                9.0,
                muted,
                Alignment::End,
            );
        }
    }

    let trade = stats.largest_trade;
    metric(
        frame,
        "Largest trade",
        trade.map_or_else(
            || "—".to_string(),
            |trade| {
                format!(
                    "{} {}",
                    if trade.is_sell { "Sell" } else { "Buy" },
                    usd(trade.notional)
                )
            },
        ),
        top + row_h * 8.0,
        trade.map_or(muted, |trade| if trade.is_sell { sell } else { buy }),
    );
    if let Some(trade) = trade {
        draw_text(
            frame,
            &format!(
                "{} · {}",
                format_price(trade.price),
                format_time(trade.time.as_u64())
            ),
            Point::new(right_value_x, top + row_h * 8.0),
            8.5,
            muted,
            Alignment::End,
        );
    }
}

fn draw_table_header(frame: &mut canvas::Frame, left: f32, width: f32, y: f32, color: Color) {
    for (label, ratio, align) in [
        ("Bid $", 0.19, Alignment::Center),
        ("Price", 0.39, Alignment::Center),
        ("Ask $", 0.57, Alignment::Center),
        ("Delta $", 0.75, Alignment::Center),
        ("Volume $", 0.97, Alignment::End),
    ] {
        draw_text(
            frame,
            label,
            Point::new(left + width * ratio, y),
            9.5,
            color,
            align,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_level_row(
    frame: &mut canvas::Frame,
    left: f32,
    width: f32,
    y: f32,
    price_units: i64,
    level: Option<LevelStats>,
    maxima: (f64, f64),
    is_traded_block: Option<bool>,
    fg: Color,
    muted: Color,
    buy: Color,
    sell: Color,
    warning: Color,
    accent: Color,
) {
    let is_traded_block = is_traded_block.unwrap_or(false);
    if is_traded_block {
        frame.fill_rectangle(
            Point::new(left + 1.0, y - 7.5),
            Size::new(width - 2.0, 15.0),
            accent.scale_alpha(0.16),
        );
        frame.fill_rectangle(
            Point::new(left + 1.0, y - 7.5),
            Size::new(2.0, 15.0),
            accent,
        );
    }
    let Some(level) = level else {
        draw_text(
            frame,
            &format_price(Price::from_units(price_units).to_f64()),
            Point::new(left + width * 0.39, y),
            9.5,
            if is_traded_block { accent } else { muted },
            Alignment::Center,
        );
        return;
    };
    let side_max = maxima.0.max(f64::EPSILON);
    let cell_w = width * 0.16;
    frame.fill_rectangle(
        Point::new(left + width * 0.11, y - 7.5),
        Size::new(cell_w * (level.bid / side_max) as f32, 15.0),
        sell.scale_alpha(0.28),
    );
    frame.fill_rectangle(
        Point::new(left + width * 0.49, y - 7.5),
        Size::new(cell_w * (level.ask / side_max) as f32, 15.0),
        buy.scale_alpha(0.28),
    );
    if maxima.1 > 0.0 && (level.volume() - maxima.1).abs() <= f64::EPSILON {
        frame.fill_rectangle(
            Point::new(left + width * 0.82, y - 7.5),
            Size::new(width * 0.17, 15.0),
            warning.scale_alpha(0.28),
        );
    }
    draw_text(
        frame,
        &usd(level.bid),
        Point::new(left + width * 0.27, y),
        9.5,
        sell,
        Alignment::End,
    );
    draw_text(
        frame,
        &format_price(Price::from_units(price_units).to_f64()),
        Point::new(left + width * 0.39, y),
        9.5,
        if is_traded_block { accent } else { fg },
        Alignment::Center,
    );
    draw_text(
        frame,
        &usd(level.ask),
        Point::new(left + width * 0.65, y),
        9.5,
        buy,
        Alignment::End,
    );
    let delta = level.delta();
    draw_text(
        frame,
        &signed_usd(delta),
        Point::new(left + width * 0.80, y),
        9.5,
        if delta >= 0.0 { buy } else { sell },
        Alignment::End,
    );
    draw_text(
        frame,
        &usd(level.volume()),
        Point::new(left + width * 0.98, y),
        9.5,
        fg,
        Alignment::End,
    );
}

fn day_price_rows(
    day: &DisplayDay,
    min_tick: PriceStep,
    block_step: Option<PriceStep>,
    max_rows: usize,
) -> (PriceStep, Vec<i64>) {
    let forced_step = block_step.map(|step| PriceStep {
        units: step.units.max(min_tick.units).max(1),
    });
    let (Some(high), Some(low)) = (day.stats.high, day.stats.low) else {
        return (forced_step.unwrap_or(min_tick), Vec::new());
    };
    let step = forced_step.unwrap_or_else(|| {
        let raw_step = ((high - low) / max_rows.max(1) as f64).max(min_tick.to_f64_lossy());
        let nice = nice_step(raw_step).max(min_tick.to_f64_lossy());
        PriceStep {
            units: Price::from_f64(nice).units.max(min_tick.units),
        }
    });
    let low_units = Price::from_f64(low).units.div_euclid(step.units) * step.units;
    let high_units = Price::from_f64(high).units.div_euclid(step.units) * step.units;
    let mut rows = Vec::new();
    let mut current = high_units;
    while current >= low_units {
        rows.push(current);
        let Some(next) = current.checked_sub(step.units) else {
            break;
        };
        current = next;
    }
    if rows.len() > max_rows {
        let focus = day
            .stats
            .last
            .map(|(_, price)| Price::from_f64(price).units.div_euclid(step.units) * step.units)
            .unwrap_or(rows[rows.len() / 2]);
        let focus_idx = rows.iter().position(|price| *price <= focus).unwrap_or(0);
        let half = max_rows / 2;
        let start = focus_idx.saturating_sub(half);
        let end = (start + max_rows).min(rows.len());
        let start = end.saturating_sub(max_rows);
        rows = rows[start..end].to_vec();
    }
    (step, rows)
}

fn price_row_count(day: &DisplayDay, min_tick: PriceStep, block_step: PriceStep) -> usize {
    let step_units = block_step.units.max(min_tick.units).max(1);
    let (Some(high), Some(low)) = (day.stats.high, day.stats.low) else {
        return 0;
    };
    let low_units = Price::from_f64(low).units.div_euclid(step_units) * step_units;
    let high_units = Price::from_f64(high).units.div_euclid(step_units) * step_units;
    let rows = (i128::from(high_units) - i128::from(low_units)) / i128::from(step_units) + 1;
    usize::try_from(rows.max(0)).unwrap_or(usize::MAX)
}

fn standalone_content_height(row_count: usize) -> f32 {
    const CHROME_HEIGHT: f32 = 218.0;
    const ROW_HEIGHT: f32 = 15.0;

    CHROME_HEIGHT + row_count as f32 * ROW_HEIGHT
}

fn day_title(index: usize) -> &'static str {
    match index {
        0 => "Current day",
        1 => "Previous day",
        2 => "Two days ago",
        _ => "Older day",
    }
}

pub(crate) fn group_levels(stats: &DayStats, step: PriceStep) -> BTreeMap<i64, LevelStats> {
    let mut grouped = BTreeMap::new();
    for (price, level) in &stats.levels {
        let bucket = price.div_euclid(step.units) * step.units;
        grouped
            .entry(bucket)
            .or_insert_with(LevelStats::default)
            .merge(*level);
    }
    grouped
}

fn nice_step(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let magnitude = 10.0_f64.powf(value.log10().floor());
    let normalized = value / magnitude;
    let factor = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    factor * magnitude
}

pub(crate) fn day_start(time: UnixMs) -> u64 {
    time.as_u64() / DAY_MS * DAY_MS
}

fn format_day(time: u64) -> String {
    chrono::DateTime::from_timestamp_millis(time as i64)
        .map(|date| date.format("%a, %d %b · UTC").to_string())
        .unwrap_or_else(|| "UTC".to_string())
}

fn format_time(time: u64) -> String {
    chrono::DateTime::from_timestamp_millis(time as i64)
        .map(|date| date.format("%H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn format_price(value: f64) -> String {
    if value.abs() >= 1_000.0 {
        format!("{value:.0}")
    } else if value.abs() >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.6}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn format_price_step(step: PriceStep) -> String {
    format!("${}", format_price(step.to_f64_lossy()))
}

fn signed(value: f64) -> String {
    if value >= 0.0 {
        format!("+{}", abbr_large_numbers(value))
    } else {
        format!("-{}", abbr_large_numbers(value.abs()))
    }
}

fn usd(value: f64) -> String {
    format!("${}", abbr_large_numbers(value))
}

pub(crate) fn signed_usd(value: f64) -> String {
    if value >= 0.0 {
        format!("+${}", abbr_large_numbers(value))
    } else {
        format!("-${}", abbr_large_numbers(value.abs()))
    }
}

pub(crate) fn draw_text(
    frame: &mut canvas::Frame,
    text: &str,
    position: Point,
    size: f32,
    color: Color,
    alignment: Alignment,
) {
    frame.fill_text(canvas::Text {
        content: text.to_string(),
        position,
        size: iced::Pixels(size),
        color,
        align_x: alignment.into(),
        align_y: Alignment::Center.into(),
        font: style::AZERET_MONO,
        ..canvas::Text::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{adapter::Exchange, unit::Qty};

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    #[test]
    fn daily_stats_match_reference_semantics() {
        let mut stats = DayStats::default();
        stats.insert_trade(trade(1_000, 100.0, 2.0, false));
        stats.insert_trade(trade(2_000, 90.0, 1.0, true));
        assert_eq!(stats.first.map(|(_, price)| price), Some(100.0));
        assert_eq!(stats.last.map(|(_, price)| price), Some(90.0));
        assert_eq!(stats.high, Some(100.0));
        assert_eq!(stats.low, Some(90.0));
        assert_eq!(stats.volume(), 290.0);
        assert_eq!(stats.delta(), 110.0);
        assert_eq!(stats.notional, 290.0);
    }

    #[test]
    fn quote_normalized_trade_is_not_multiplied_by_price_twice() {
        let mut stats = DayStats::default();
        stats.insert_trade_with_unit(trade(1_000, 100.0, 200.0, false), DayBookRetain::ALL, true);
        assert_eq!(stats.notional, 200.0);
        assert_eq!(stats.buy, 200.0);
    }

    fn stored_print(time: u64, notional: f64, is_sell: bool) -> StoredLargeTrade {
        StoredLargeTrade {
            time: UnixMs::new(time),
            price_units: 1,
            notional,
            is_sell,
        }
    }

    #[test]
    fn large_trade_cap_keeps_the_whale_instead_of_the_oldest_small_prints() {
        let mut stats = DayStats::default();
        for i in 0..MAX_LARGE_TRADES_PER_DAY {
            stats.push_large_trade(stored_print(i as u64, 10_000.0, false));
        }
        // The rally print lands in the middle of the session.
        stats.push_large_trade(stored_print(50_000, 2_000_000.0, false));
        for i in 0..MAX_LARGE_TRADES_PER_DAY {
            stats.push_large_trade(stored_print(100_000 + i as u64, 11_000.0, true));
        }
        stats.compact_large_trades();

        assert!(
            stats
                .large_trades
                .iter()
                .any(|trade| (trade.notional - 2_000_000.0).abs() < f64::EPSILON),
            "a $2M rally print must survive a day of $10k tape"
        );
        assert!(stats.large_trades.len() <= MAX_LARGE_TRADES_PER_DAY);
        assert!(
            stats
                .large_trades
                .iter()
                .all(|trade| trade.notional >= 11_000.0)
        );
    }

    #[test]
    fn display_merge_does_not_copy_large_trades() {
        let mut left = DayStats::default();
        let mut right = DayStats::default();
        left.push_large_trade(stored_print(1, 2_000_000.0, false));
        right.push_large_trade(stored_print(2, 3_000_000.0, true));
        left.merge(&right);

        assert_eq!(left.large_trades.len(), 1);
        assert!((left.large_trades[0].notional - 2_000_000.0).abs() < f64::EPSILON);
        assert_eq!(left.volume(), 0.0);
    }

    #[test]
    fn insert_trade_retains_a_million_dollar_print_on_a_busy_btc_day() {
        let mut stats = DayStats::default();
        let small_qty = 1_000_000.0 / 80_000.0;
        for i in 0..MAX_LARGE_TRADES_PER_DAY {
            stats.insert_trade(trade(i as u64, 80_000.0, small_qty, false));
        }
        stats.insert_trade(trade(50_000, 80_000.0, 50.0, false));
        let later_qty = 1_100_000.0 / 80_000.0;
        for i in 0..MAX_LARGE_TRADES_PER_DAY {
            stats.insert_trade(trade(100_000 + i as u64, 80_000.0, later_qty, true));
        }
        stats.compact_large_trades();

        let whale = 80_000.0 * 50.0;
        assert!(
            stats
                .large_trades
                .iter()
                .any(|trade| (trade.notional - whale).abs() < 1.0),
            "price * qty of 50 BTC at 80k must remain after the $1M floor floods the cap"
        );
        assert!(stats.large_trades.len() <= MAX_LARGE_TRADES_PER_DAY);
    }

    #[test]
    fn source_history_keeps_venues_independent_until_display_merge() {
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
        let now = UnixMs::now();
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.on_source_trades(binance, &[trade(now.as_u64(), 100.0, 2.0, false)], false);
        indicator.on_source_trades(bybit, &[trade(now.as_u64(), 100.0, 1.0, true)], false);
        let days = indicator.display_days(now);
        assert_eq!(days[0].stats.volume(), 300.0);
        assert_eq!(days[0].stats.delta(), 100.0);
    }

    #[test]
    fn bounded_display_refresh_keeps_authoritative_live_books_exact() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let now = UnixMs::now();
        let mut indicator = FootprintHistoryIndicator::new_display();
        indicator.configure_footprint_history(&[source], true);
        indicator.on_source_trades(
            source,
            &[
                trade(now.as_u64(), 100.0, 2.0, false),
                trade(now.as_u64().saturating_add(1), 100.0, 1.0, true),
            ],
            false,
        );

        assert_eq!(
            indicator.display_day_at(day_start(now)).stats.volume(),
            300.0
        );
        assert!(indicator.has_pending_display_refresh());
        assert!(indicator.flush_display_if_due(true));
        assert!(!indicator.has_pending_display_refresh());
        assert_eq!(indicator.display[0].stats.volume(), 300.0);
    }

    #[test]
    fn failed_streamed_request_retry_commits_each_trade_once() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let first = trade(day + 1_000, 100.0, 2.0, false);
        let second = trade(day + 2_000, 90.0, 1.0, true);
        let failed_id = uuid::Uuid::new_v4();
        let retry_id = uuid::Uuid::new_v4();

        let mut actual = FootprintHistoryIndicator::new();
        actual.configure_footprint_history(&[source], true);
        actual.stage_source_trades(failed_id, source, &[first]);
        actual.discard_staged_source_trades(failed_id);
        assert_eq!(actual.display_day_at(day).stats, DayStats::default());
        actual.stage_source_trades(retry_id, source, &[first]);
        actual.stage_source_trades(retry_id, source, &[second]);
        actual.commit_staged_source_trades(retry_id);

        let mut expected = FootprintHistoryIndicator::new();
        expected.configure_footprint_history(&[source], true);
        expected.on_source_trades(source, &[first, second], true);
        assert_eq!(
            actual.display_day_at(day).stats,
            expected.display_day_at(day).stats
        );
    }

    #[test]
    fn delayed_live_start_advances_the_history_seam() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let provisional_cutoff = UnixMs::new(day + 10_000);
        let first_live = UnixMs::new(day + 2 * ONE_MIN_MS + 10_000);
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator.prepare_footprint_history(source, provisional_cutoff);
        indicator.on_source_trades(
            source,
            &[trade(first_live.as_u64(), 10.0, 20.0, false)],
            false,
        );

        indicator.prepare_footprint_history(source, first_live.saturating_sub(1));
        let req_id = uuid::Uuid::new_v4();
        indicator.stage_source_trades(
            req_id,
            source,
            &[trade(day + ONE_MIN_MS + 10_000, 10.0, 10.0, false)],
        );
        indicator.commit_staged_source_trades(req_id);
        indicator.accept_verified_day(source, UnixMs::new(day), first_live.saturating_sub(1));

        let stats = indicator.display_day_at(day).stats;
        assert_eq!(stats.volume(), 300.0);
        assert_eq!(stats.delta(), 300.0);
    }

    #[test]
    fn live_seam_clips_only_the_recorder_suffix_it_replaces() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let first_live = UnixMs::new(day + 10 * ONE_MIN_MS);
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], false);
        indicator.on_source_trades(
            source,
            &[trade(first_live.as_u64(), 10.0, 20.0, false)],
            false,
        );
        indicator.mark_incomplete_trade_history(
            source,
            &[(
                UnixMs::new(day + 8 * ONE_MIN_MS),
                UnixMs::new(day + 12 * ONE_MIN_MS),
            )],
        );

        assert_eq!(
            indicator
                .incomplete_ranges
                .get(&(source.ticker, day))
                .map(Vec::as_slice),
            Some([(day + 8 * ONE_MIN_MS, first_live.saturating_sub(1).as_u64())].as_slice())
        );
    }

    #[test]
    fn source_toggles_recompose_cached_history_without_recalculation() {
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
        let now = UnixMs::now();
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.on_source_trades(binance, &[trade(now.as_u64(), 100.0, 2.0, false)], false);
        indicator.on_source_trades(bybit, &[trade(now.as_u64(), 100.0, 1.0, true)], false);

        indicator.configure_footprint_history(&[binance], true);
        assert_eq!(indicator.display_days(now)[0].stats.volume(), 200.0);

        indicator.configure_footprint_history(&[binance, bybit], true);
        assert_eq!(indicator.display_days(now)[0].stats.volume(), 300.0);
    }

    #[test]
    fn requested_aggregate_history_stays_hidden_until_every_source_is_verified() {
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
        let day = day_start(UnixMs::now());
        let cutoff = UnixMs::new(day + 500);
        let through = UnixMs::new(day + 10_000);
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.prepare_footprint_history(binance, cutoff);
        indicator.prepare_footprint_history(bybit, cutoff);
        indicator.on_source_trades(binance, &[trade(day + 1_000, 100.0, 2.0, false)], false);
        indicator.on_source_trades(bybit, &[trade(day + 2_000, 100.0, 1.0, true)], false);

        indicator
            .cache_checkpoints
            .insert((binance.ticker, day), Some(through));
        assert_eq!(indicator.display_day_at(day).stats, DayStats::default());
        assert!(indicator.merged_cvd_deltas_from(day, day).is_empty());

        indicator
            .cache_checkpoints
            .insert((bybit.ticker, day), Some(through));
        assert_eq!(indicator.display_day_at(day).stats.volume(), 300.0);
    }

    #[test]
    fn utc_days_covering_keeps_a_scrolled_back_day() {
        let from = UnixMs::new(5 * DAY_MS);
        let to = UnixMs::new(5 * DAY_MS + 12 * 3_600_000);
        let ranges = utc_days_covering(from, to, 5);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].0, from);
        assert_eq!(ranges[0].1, to);
    }

    #[test]
    fn utc_day_ranges_cover_four_full_days_clipped_to_cutoff() {
        let cutoff = UnixMs::new(5 * DAY_MS + 1234);
        let ranges = utc_day_ranges(cutoff, 4);
        assert_eq!(ranges.len(), 4);
        assert_eq!(
            ranges[0],
            (UnixMs::new(2 * DAY_MS), UnixMs::new(3 * DAY_MS - 1))
        );
        assert_eq!(
            ranges[1],
            (UnixMs::new(3 * DAY_MS), UnixMs::new(4 * DAY_MS - 1))
        );
        assert_eq!(
            ranges[2],
            (UnixMs::new(4 * DAY_MS), UnixMs::new(5 * DAY_MS - 1))
        );
        assert_eq!(ranges[3], (UnixMs::new(5 * DAY_MS), cutoff));
    }

    #[test]
    fn cached_day_resumes_after_its_checkpoint() {
        let start = UnixMs::new(5 * DAY_MS);
        let end = UnixMs::new(6 * DAY_MS - 1);
        let checkpoint = UnixMs::new(5 * DAY_MS + 12_345);

        assert_eq!(
            missing_day_range(start, end, Some(checkpoint)),
            Some((checkpoint.saturating_add(1), end))
        );
        assert_eq!(missing_day_range(start, end, Some(end)), None);
        assert_eq!(missing_day_range(start, end, None), Some((start, end)));
    }

    #[test]
    fn completed_day_cache_round_trips_derived_stats() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = DAY_MS;
        let mut stats = DayStats::default();
        stats.insert_trade(trade(day + 1_000, 100.0, 2.0, false));
        stats.insert_trade(trade(day + 2_000, 90.0, 1.0, true));
        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-cache-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.json");

        let covered_through = UnixMs::new(day + 12_345);
        FootprintHistoryIndicator::write_cached_day(&path, source, day, covered_through, &stats)
            .expect("write cache");
        let (loaded, loaded_through) =
            FootprintHistoryIndicator::read_cached_day(&path, source, day).expect("read cache");

        let mut expected = stats;
        expected.one_min_delta.clear();
        assert_eq!(loaded, expected);
        assert_eq!(loaded_through, covered_through);
        std::fs::remove_file(cache_proof_path(&path)).expect("remove completion proof");
        assert!(FootprintHistoryIndicator::read_cached_day(&path, source, day).is_none());
        assert!(
            path.exists(),
            "unproven cache bytes must be preserved for recovery"
        );
        std::fs::remove_dir_all(&root).expect("remove cache directory");
    }

    #[test]
    fn repeated_cache_hydration_preserves_newer_in_memory_trades() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let mut cached = DayStats::default();
        cached.insert_trade(trade(day + 1_000, 100.0, 2.0, false));
        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-cache-hydration-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.json");
        let covered_through = UnixMs::new(day + 12_345);
        FootprintHistoryIndicator::write_cached_day(&path, source, day, covered_through, &cached)
            .expect("write cache");

        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        assert_eq!(
            indicator.load_day_cache_from_path(source, day, &path),
            Some(covered_through)
        );
        indicator.on_source_trades(source, &[trade(day + 2_000, 101.0, 3.0, false)], true);
        assert_eq!(indicator.display_day_at(day).stats.volume(), 503.0);

        assert_eq!(
            indicator.load_day_cache_from_path(source, day, &path),
            Some(covered_through)
        );
        assert_eq!(indicator.display_day_at(day).stats.volume(), 503.0);

        std::fs::remove_dir_all(&root).expect("remove cache directory");
    }

    #[test]
    fn first_cache_hydration_preserves_preexisting_post_cutoff_live_trade() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let cutoff = UnixMs::new(day + 10_000);
        let mut cached = DayStats::default();
        cached.insert_trade(trade(day + 1_000, 100.0, 2.0, false));
        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-first-hydration-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.fpbin");
        FootprintHistoryIndicator::write_cached_day(&path, source, day, cutoff, &cached)
            .expect("write cache");

        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator.on_source_trades(source, &[trade(day + 11_000, 101.0, 3.0, false)], false);
        assert_eq!(
            indicator.history_cutoffs.get(&source.ticker),
            Some(&UnixMs::new(day + 10_999))
        );
        assert_eq!(
            indicator.load_day_cache_from_path(source, day, &path),
            Some(cutoff)
        );
        assert_eq!(indicator.display_day_at(day).stats.volume(), 503.0);

        std::fs::remove_dir_all(&root).expect("remove cache directory");
    }

    #[test]
    fn period_coverage_requires_every_day_from_every_active_venue() {
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
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        let start = 10 * DAY_MS;
        let end = 16 * DAY_MS + DAY_MS - 1;

        for offset in 0..7 {
            indicator
                .completed_days
                .insert((binance.ticker, start + offset * DAY_MS));
        }
        assert_eq!(
            indicator.period_coverage(UnixMs::new(start), UnixMs::new(end)),
            (7, 14)
        );

        for offset in 0..7 {
            indicator
                .completed_days
                .insert((bybit.ticker, start + offset * DAY_MS));
        }
        assert_eq!(
            indicator.period_coverage(UnixMs::new(start), UnixMs::new(end)),
            (14, 14)
        );
    }

    #[test]
    fn complete_period_merge_does_not_include_a_partially_loaded_venue() {
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
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        let start = 10 * DAY_MS;
        let end = 16 * DAY_MS + DAY_MS - 1;

        indicator
            .histories
            .entry(binance.ticker)
            .or_default()
            .days
            .entry(start)
            .or_default()
            .insert_trade(trade(start + 1_000, 100.0, 2.0, false));
        indicator
            .histories
            .entry(bybit.ticker)
            .or_default()
            .days
            .entry(start)
            .or_default()
            .insert_trade(trade(start + 1_000, 100.0, 3.0, false));
        for offset in 0..7 {
            indicator
                .completed_days
                .insert((binance.ticker, start + offset * DAY_MS));
        }
        indicator.completed_days.insert((bybit.ticker, start));

        let (binance_only, complete_sources) =
            indicator.display_complete_period(UnixMs::new(start), UnixMs::new(end));
        assert_eq!(complete_sources, 1);
        assert_eq!(binance_only.volume(), 200.0);

        for offset in 1..7 {
            indicator
                .completed_days
                .insert((bybit.ticker, start + offset * DAY_MS));
        }
        let (aggregate, complete_sources) =
            indicator.display_complete_period(UnixMs::new(start), UnixMs::new(end));
        assert_eq!(complete_sources, 2);
        assert_eq!(aggregate.volume(), 500.0);
    }

    #[test]
    fn fetching_one_day_preserves_already_loaded_days() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let today = day_start(UnixMs::now());
        let older_day = today.saturating_sub(2 * DAY_MS);
        let newer_day = today.saturating_sub(DAY_MS);
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator
            .histories
            .entry(source.ticker)
            .or_default()
            .days
            .entry(older_day)
            .or_default()
            .insert_trade(trade(older_day + 1_000, 100.0, 1.0, false));

        indicator.on_source_trades(source, &[trade(newer_day + 1_000, 110.0, 1.0, false)], true);

        let history = indicator.histories.get(&source.ticker).expect("history");
        assert!(history.days.contains_key(&older_day));
        assert!(history.days.contains_key(&newer_day));
    }

    #[test]
    fn short_lookback_skips_wholly_irrelevant_historical_batches() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator.set_lookback_days(1);

        indicator.on_source_trades(source, &[trade(DAY_MS, 100.0, 1.0, false)], true);

        assert!(indicator.histories.is_empty());
        assert!(indicator.historical_days_started.is_empty());
    }

    #[test]
    fn live_captures_survive_their_days_first_historical_batch() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        let now = UnixMs::now();
        let today = day_start(now);
        // A cutoff guaranteed to fall inside today, whatever the wall clock.
        let cutoff = UnixMs::new(today.max(now.as_u64().saturating_sub(60_000)));
        indicator.prepare_footprint_history(source, cutoff);

        // Live prints captured after startup accumulate into today's book.
        indicator.on_source_trades(
            source,
            &[trade(cutoff.as_u64() + 1_000, 110.0, 2.0, false)],
            false,
        );

        // The authoritative backfill for today finally arrives (it is paced
        // last, after every older UTC day) and must not wipe the live seam.
        indicator.on_source_trades(source, &[trade(today + 1_000, 90.0, 3.0, true)], true);
        indicator.accept_verified_day(source, UnixMs::new(today), cutoff);

        let stats = indicator.display_day_at(today).stats;
        assert_eq!(stats.volume(), 490.0);
        assert_eq!(stats.delta(), -50.0);
    }

    #[test]
    fn late_live_captures_survive_backfills_that_started_on_another_day() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        let today = day_start(UnixMs::now());
        let yesterday = today.saturating_sub(DAY_MS);
        // A cutoff guaranteed to fall inside today, whatever the wall clock.
        let cutoff = UnixMs::new(today.max(UnixMs::now().as_u64().saturating_sub(60_000)));
        indicator.prepare_footprint_history(source, cutoff);

        // Backfill starts on an older day.
        indicator.on_source_trades(source, &[trade(yesterday + 1_000, 100.0, 1.0, false)], true);

        // Live prints captured while backfill grinds through the lookback.
        indicator.on_source_trades(
            source,
            &[trade(cutoff.as_u64() + 2_000, 120.0, 2.0, true)],
            false,
        );

        // Today's batch arrives much later; the live capture must survive it
        // exactly once.
        indicator.on_source_trades(source, &[trade(today + 5_000, 90.0, 4.0, true)], true);
        indicator.accept_verified_day(source, UnixMs::new(today), cutoff);

        let stats = indicator.display_day_at(today).stats;
        assert_eq!(stats.volume(), 600.0);
        assert_eq!(stats.delta(), -600.0);
    }

    #[test]
    fn each_day_builds_its_own_price_ladder() {
        let mut current = DisplayDay::default();
        current.stats.insert_trade(trade(1_000, 100.0, 1.0, false));
        let mut older = DisplayDay::default();
        older.stats.insert_trade(trade(1_000, 200.0, 1.0, false));
        let step = PriceStep {
            units: Price::from_f64(1.0).units,
        };

        let (_, current_rows) = day_price_rows(&current, step, Some(step), 10);
        let (_, older_rows) = day_price_rows(&older, step, Some(step), 10);

        assert_eq!(
            current_rows.first().copied(),
            Some(Price::from_f64(100.0).units)
        );
        assert_eq!(
            older_rows.first().copied(),
            Some(Price::from_f64(200.0).units)
        );
    }

    #[test]
    fn standalone_height_grows_to_show_the_full_price_ladder() {
        let mut day = DisplayDay::default();
        day.stats.insert_trade(trade(1_000, 100.0, 1.0, false));
        day.stats.insert_trade(trade(2_000, 90.0, 1.0, true));
        let step = PriceStep {
            units: Price::from_f64(1.0).units,
        };

        let rows = price_row_count(&day, step, step);

        assert_eq!(rows, 11);
        assert_eq!(standalone_content_height(rows), 383.0);
    }

    #[test]
    fn large_trades_only_insert_skips_price_levels() {
        let mut stats = DayStats::default();
        stats.insert_trade_with(
            trade(1_000, 80_000.0, 20.0, false),
            DayBookRetain::LARGE_TRADES_ONLY,
        );
        assert!(stats.levels.is_empty());
        assert!(stats.five_min_delta.is_empty());
        assert_eq!(stats.large_trades.len(), 1);
        assert!((stats.volume() - 1_600_000.0).abs() < 1.0);
    }

    #[test]
    fn incomplete_day_books_are_not_written_to_the_shared_cache() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = DAY_MS;
        let mut stats = DayStats::default();
        stats.insert_trade_with(
            trade(day + 1_000, 80_000.0, 20.0, false),
            DayBookRetain::LARGE_TRADES_ONLY,
        );
        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-cache-incomplete-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.fpbin");

        FootprintHistoryIndicator::write_cached_day(
            &path,
            source,
            day,
            UnixMs::new(day + 12_345),
            &stats,
        )
        .expect("skip incomplete write");
        assert!(
            !path.exists(),
            "large-trades-only books must not occupy the shared cache path"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn display_merge_combines_five_minute_deltas() {
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
        let today = day_start(UnixMs::now());
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.on_source_trades(binance, &[trade(today + 60_000, 10.0, 100.0, false)], true);
        indicator.on_source_trades(bybit, &[trade(today + 60_000, 10.0, 50.0, true)], true);

        let merged = indicator.display_day_at(today).stats.five_min_delta;
        let bucket = today / FIVE_MIN_MS * FIVE_MIN_MS;
        assert!((merged[&bucket] - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cvd_merge_keeps_cached_five_minute_truth_and_adds_live_minute_once() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let today = day_start(UnixMs::now());
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], false);

        let mut cached = DayStats::default();
        cached.five_min_delta.insert(today, 500.0);
        indicator
            .histories
            .entry(source.ticker)
            .or_default()
            .days
            .insert(today, cached);

        // A new $100 buy is present in both the regular five-minute map and
        // the session-local minute map. The CVD merge must subtract it from
        // the coarse bucket before placing it at its real minute timestamp.
        indicator.on_source_trades(
            source,
            &[trade(today + ONE_MIN_MS + 1_000, 10.0, 10.0, false)],
            false,
        );

        let merged = indicator.merged_cvd_deltas_from(today, today);
        assert_eq!(merged[&today], 500.0);
        assert_eq!(merged[&(today + ONE_MIN_MS)], 100.0);
        assert_eq!(merged.values().sum::<f64>(), 600.0);

        let suffix = indicator.merged_cvd_deltas_from(today, today + ONE_MIN_MS);
        assert_eq!(suffix.len(), 1);
        assert_eq!(suffix[&(today + ONE_MIN_MS)], 100.0);
    }

    fn whale(day: u64) -> Trade {
        trade(day + 5 * 60 * 60 * 1_000 + 10_000, 77_700.0, 197.0, true)
    }

    #[test]
    fn cvd_insert_still_captures_prints_for_the_shared_cache() {
        let mut stats = DayStats::default();
        stats.insert_trade_with(
            trade(1_000, 80_000.0, 20.0, false),
            DayBookRetain::DELTAS_WITHOUT_PRINTS,
        );
        assert_eq!(stats.levels.len(), 1);
        assert_eq!(stats.large_trades.len(), 1);
        assert!((stats.large_trades[0].notional - 1_600_000.0).abs() < 1.0);
    }

    #[test]
    fn compact_cvd_staging_never_builds_price_or_print_maps() {
        let mut stats = DayStats::default();
        for index in 0..20_000_u64 {
            stats.insert_trade_with(
                trade(
                    index * 1_000,
                    80_000.0 + (index % 5_000) as f64,
                    20.0,
                    false,
                ),
                DayBookRetain::CVD_DELTAS_ONLY,
            );
        }

        assert!(stats.levels.is_empty());
        assert!(stats.large_trades.is_empty());
        assert!(stats.one_min_delta.len() <= 334);
        assert!(stats.five_min_delta.len() <= 67);
    }

    #[test]
    fn compact_consumer_advances_coverage_without_writing_shared_cache() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = day_start(UnixMs::now());
        let covered_through = UnixMs::new(day + 60_000);
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.set_day_book_retain(DayBookRetain::CVD_DELTAS_ONLY);
        indicator.configure_footprint_history(&[source], false);
        indicator.on_source_trades(source, &[trade(day + 1_000, 80_000.0, 1.0, false)], true);

        indicator.persist_day_cache(source, UnixMs::new(day), covered_through);

        assert_eq!(
            indicator.cache_checkpoints[&(source.ticker, day)],
            Some(covered_through)
        );
        assert!(
            indicator.histories[&source.ticker].days[&day]
                .levels
                .is_empty()
        );
    }

    #[test]
    fn cvd_style_persist_does_not_wipe_existing_large_trades() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = DAY_MS;
        let mut with_prints = DayStats::default();
        with_prints.insert_trade(whale(day));
        let mut without_prints = with_prints.clone();
        without_prints.large_trades.clear();

        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-cache-cvd-wipe-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.fpbin");
        FootprintHistoryIndicator::write_cached_day(
            &path,
            source,
            day,
            UnixMs::new(day + 12_345),
            &with_prints,
        )
        .expect("daily-delta write");
        FootprintHistoryIndicator::write_cached_day(
            &path,
            source,
            day,
            UnixMs::new(day + 43_200_000),
            &without_prints,
        )
        .expect("cvd extension write");

        let (loaded, covered) =
            FootprintHistoryIndicator::read_cached_day(&path, source, day).expect("read");
        assert_eq!(covered, UnixMs::new(day + 43_200_000));
        assert_eq!(loaded.large_trades.len(), 1);
        assert!(
            (loaded.large_trades[0].notional - 15_306_900.0).abs() < 1.0,
            "got {}",
            loaded.large_trades[0].notional
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn same_coverage_printless_snapshot_does_not_replace_whales() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let day = DAY_MS;
        let mut with_prints = DayStats::default();
        with_prints.insert_trade(whale(day));
        let mut without_prints = with_prints.clone();
        without_prints.large_trades.clear();
        let covered = UnixMs::new(day + 12_345);

        let root = std::env::temp_dir().join(format!(
            "flowsurface-footprint-cache-cvd-same-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("day.fpbin");
        FootprintHistoryIndicator::write_cached_day(&path, source, day, covered, &with_prints)
            .expect("daily-delta write");
        FootprintHistoryIndicator::write_cached_day(&path, source, day, covered, &without_prints)
            .expect("cvd same-coverage write");

        let (loaded, _) =
            FootprintHistoryIndicator::read_cached_day(&path, source, day).expect("read");
        assert_eq!(loaded.large_trades.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn partial_verified_day_exposes_individual_prints_but_not_daily_totals() {
        let source = TickerInfo::new(
            Ticker::new("BTC", Exchange::HyperliquidLinear),
            1.0,
            0.00001,
            None,
        );
        let day = day_start(UnixMs::now());
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[source], false);
        indicator.prepare_footprint_history(source, UnixMs::new(day + DAY_MS - 1));
        indicator.accept_verified_day(source, UnixMs::new(day), UnixMs::new(day + 2 * 60_000 - 1));
        let req_id = uuid::Uuid::new_v4();
        indicator.stage_source_trades(
            req_id,
            source,
            &[trade(day + 4 * 60_000, 80_000.0, 25.0, false)],
        );
        indicator.commit_staged_source_trades(req_id);
        indicator.mark_incomplete_trade_history(
            source,
            &[(UnixMs::new(day + 2 * 60_000), UnixMs::new(day + 3 * 60_000))],
        );

        assert_eq!(indicator.display_day_at(day).stats, DayStats::default());
        assert_eq!(indicator.display_large_trades(day, 1_000_000.0).len(), 1);
        assert_eq!(
            indicator.cvd_missing_ranges(day),
            vec![(day + 2 * 60_000, day + 3 * 60_000)]
        );
    }
}
