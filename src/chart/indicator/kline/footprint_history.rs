use crate::{
    chart::{Caches, Interaction, Message, ViewState, indicator::kline::KlineIndicatorImpl},
    connector::fetcher::{TradeFetchMode, trade_fetch_mode},
    style,
};

use data::{chart::PlotData, chart::kline::KlineDataPoint, util::abbr_large_numbers};
use exchange::{
    OpenInterest, SizeUnit, Ticker, TickerInfo, Trade, UnixMs,
    adapter::Venue,
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

pub(crate) const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
const FIVE_MIN_MS: u64 = 5 * 60 * 1_000;
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
pub(crate) const CACHE_SCHEMA_VERSION: u16 = 5;
const MAX_LOOKBACK_DAYS: usize = 732;

/// Notional floor (quote currency) at which an executed trade is retained for
/// the Large Trades overlay. Matches the lowest configurable UI threshold so
/// lowering the threshold never requires a re-backfill within retention.
fn large_trades_capture_floor() -> f64 {
    f64::from(data::chart::kline::Config::LARGE_TRADES_MIN_USD_MIN)
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
    pub five_min_delta: bool,
    pub large_trades: bool,
}

impl DayBookRetain {
    pub const ALL: Self = Self {
        levels: true,
        five_min_delta: true,
        large_trades: true,
    };
    /// Large Trades overlay: keep prints, skip price-level and CVD maps.
    pub const LARGE_TRADES_ONLY: Self = Self {
        levels: false,
        five_min_delta: false,
        large_trades: true,
    };
    /// CVD subplot: keep five-minute buckets (and levels so the shared disk
    /// cache stays complete for Footprint History), skip individual prints.
    pub const DELTAS_WITHOUT_PRINTS: Self = Self {
        levels: true,
        five_min_delta: true,
        large_trades: false,
    };
}

impl DayStats {
    #[cfg(test)]
    fn insert_trade(&mut self, trade: Trade) {
        self.insert_trade_with(trade, DayBookRetain::ALL);
    }

    fn insert_trade_with(&mut self, trade: Trade, retain: DayBookRetain) {
        let price = trade.price.to_f64();
        let qty = trade.qty.to_f64();
        let notional = price * qty;

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

        if retain.five_min_delta {
            let bucket = trade.time.as_u64() / FIVE_MIN_MS * FIVE_MIN_MS;
            *self.five_min_delta.entry(bucket).or_default() +=
                if trade.is_sell { -notional } else { notional };
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

        if retain.large_trades && notional >= large_trades_capture_floor() {
            self.push_large_trade(StoredLargeTrade {
                time: trade.time,
                price_units: trade.price.units,
                notional,
                is_sell: trade.is_sell,
            });
        }
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

    /// Five-minute USD-delta buckets (`bucket_start_ms -> buy - sell notional`)
    /// shared with the aggregated CVD subplot.
    pub(crate) fn five_min_deltas(&self) -> &BTreeMap<u64, f64> {
        &self.five_min_delta
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
    historical_days_started: FxHashSet<(Ticker, u64)>,
    /// Days whose book holds live trades captured after the backfill cutoff.
    /// Their first historical batch merges instead of rebuilding so the live
    /// seam is never destroyed (historical ranges never exceed the cutoff).
    live_seam_days: FxHashSet<(Ticker, u64)>,
    completed_days: FxHashSet<(Ticker, u64)>,
    cache_checkpoints: FxHashMap<(Ticker, u64), Option<UnixMs>>,
    retain: DayBookRetain,
    /// When true, keep a merged 3-day display snapshot so iced `view()` does
    /// not clone/merge venue level maps on every mouse move.
    track_display: bool,
    display: Box<[DisplayDay; DAYS]>,
    display_today: u64,
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
            historical_days_started: FxHashSet::default(),
            live_seam_days: FxHashSet::default(),
            completed_days: FxHashSet::default(),
            cache_checkpoints: FxHashMap::default(),
            retain: DayBookRetain::ALL,
            track_display: false,
            display: Box::default(),
            display_today: 0,
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
        if stats.notional > 0.0 && stats.levels.is_empty() {
            return Ok(());
        }
        if Self::read_cached_day(path, source, day)
            .is_some_and(|(_, cached_through)| cached_through >= covered_through)
        {
            return Ok(());
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
            stats: stats.clone(),
        };
        let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard())
            .map_err(std::io::Error::other)?;
        let temp_path = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp_path, bytes)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        if let Err(err) = std::fs::rename(&temp_path, path) {
            let target_won_race = path.exists();
            let _ = std::fs::remove_file(&temp_path);
            if !target_won_race {
                return Err(err);
            }
        }
        Ok(())
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
        if !self.retain.five_min_delta {
            stats.five_min_delta.clear();
        }
        if !self.retain.large_trades {
            stats.large_trades.clear();
        }
        self.histories
            .entry(source.ticker)
            .or_default()
            .days
            .insert(day, stats);
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
        let Some(stats) = self
            .histories
            .get_mut(&source.ticker)
            .and_then(|history| history.days.get_mut(&day))
        else {
            return;
        };
        stats.compact_large_trades();
        if covered_through.as_u64() >= day.saturating_add(DAY_MS).saturating_sub(1) {
            self.completed_days.insert((source.ticker, day));
        }
        if !self.retain.levels {
            // Incomplete books (Large Trades skips price levels) must not
            // occupy the shared on-disk path: Footprint History would treat
            // an empty-level file as a complete day and skip the real fetch.
            return;
        }
        let path = Self::cache_path(source, day);
        // Encoding a busy day still costs tens of milliseconds and the write
        // itself blocks on disk — keep both off the UI thread. The clone is
        // the only main-thread cost and is far cheaper than encoding.
        let stats = stats.clone();
        self.cache_checkpoints
            .insert((source.ticker, day), Some(covered_through));
        std::thread::Builder::new()
            .name("footprint-cache-writer".to_string())
            .spawn(move || {
                if let Err(err) =
                    Self::write_cached_day(&path, source, day, covered_through, &stats)
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

    fn retain_oldest(&self, now: UnixMs) -> u64 {
        day_start(now).saturating_sub((self.lookback_days.saturating_sub(1) as u64) * DAY_MS)
    }

    /// Drop the per-price-level detail of one UTC day across every source,
    /// keeping only the aggregates (five-minute delta buckets, totals). Used
    /// by cumulative-style consumers after a day is safely persisted or
    /// loaded complete, so long lookbacks do not retain level maps per venue.
    /// In-memory only: disk caches are written before this runs and stay
    /// complete for other consumers.
    pub(crate) fn slim_day_levels(&mut self, day: u64) {
        let day = day_start(UnixMs::new(day));
        for history in self.histories.values_mut() {
            if let Some(stats) = history.days.get_mut(&day) {
                stats.levels.clear();
            }
        }
        self.rebuild_display();
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

    #[cfg(test)]
    pub(crate) fn display_days(&self, now: UnixMs) -> [DisplayDay; DAYS] {
        let days = self.display_day_list(now, DAYS);
        std::array::from_fn(|index| days.get(index).cloned().unwrap_or_default())
    }

    pub(crate) fn display_day_at(&self, day: u64) -> DisplayDay {
        let mut result = DisplayDay::default();
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

    /// Venue-merged five-minute USD-delta buckets for one UTC day.
    ///
    /// Avoids `display_day_at`, which also clones and merges per-price level
    /// maps CVD never reads.
    pub(crate) fn merged_five_min_deltas(&self, day: u64) -> BTreeMap<u64, f64> {
        let mut merged = BTreeMap::new();
        for source in self.active_sources() {
            let Some(deltas) = self
                .histories
                .get(&source.ticker)
                .and_then(|history| history.days.get(&day))
                .map(DayStats::five_min_deltas)
            else {
                continue;
            };
            for (bucket, delta) in deltas {
                *merged.entry(*bucket).or_default() += *delta;
            }
        }
        merged
    }

    /// Visit large executed trades for a UTC day across active sources.
    /// Does not allocate; the overlay keeps only the markers it will draw.
    pub(crate) fn for_each_large_trade(
        &self,
        day_start_ts: u64,
        threshold_usd: f32,
        mut visit: impl FnMut(StoredLargeTrade),
    ) {
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
        self.live_seam_days.clear();
        self.completed_days.clear();
        self.cache_checkpoints.clear();
        self.histories.clear();
        self.history_cutoffs.clear();
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
        self.history_cutoffs.insert(source.ticker, cutoff);
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

        if historical {
            let touched_days = trades
                .iter()
                .map(|trade| day_start(trade.time))
                .collect::<FxHashSet<_>>();
            let history = self.histories.entry(source_key).or_default();
            for day in touched_days {
                if self.historical_days_started.insert((source_key, day)) {
                    // Authoritative history replaces whatever the day held,
                    // unless the book already captured live trades from after
                    // the backfill cutoff. Historical fetch ranges are clipped
                    // at that cutoff, so those live captures cannot duplicate
                    // the batch and must survive it: backfill is paced one UTC
                    // day at a time over the whole lookback, so today's batch
                    // can arrive hours into a session and wiping here would
                    // silently delete every large print seen in between.
                    if !self.live_seam_days.remove(&(source_key, day)) {
                        history.days.remove(&day);
                    }
                }
            }
        }

        let cutoff = self.history_cutoffs.get(&source_key).copied();
        let retain = self.retain;
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
                    .insert_trade_with(*trade, retain);
            }
            history.days.retain(|day, _| *day >= oldest);
        }
        self.rebuild_display();
        self.clear_all_caches();
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

        assert_eq!(loaded, stats);
        assert_eq!(loaded_through, covered_through);
        std::fs::remove_file(&path).expect("remove cache file");
        std::fs::remove_dir(&root).expect("remove cache directory");
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

        std::fs::remove_file(&path).expect("remove cache file");
        std::fs::remove_dir(&root).expect("remove cache directory");
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
    fn five_min_delta_merge_does_not_need_level_maps() {
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

        let merged = indicator.merged_five_min_deltas(today);
        let via_display = indicator
            .display_day_at(today)
            .stats
            .five_min_deltas()
            .clone();
        assert_eq!(merged, via_display);
        let bucket = today / FIVE_MIN_MS * FIVE_MIN_MS;
        assert!((merged[&bucket] - 500.0).abs() < f64::EPSILON);
    }
}
