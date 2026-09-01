use super::KlineIndicatorImpl;
use super::footprint_history::{
    CvdSourceDay, DAY_MS, DELTA_ROUNDING_EPSILON, DayBookRetain, FootprintHistoryIndicator,
    cache_has_completion_proof, day_start, trade_notional, write_cache_with_completion_proof,
};
use crate::chart::{
    Basis, Caches, Message, ViewState,
    indicator::{
        indicator_row,
        kline::{AvailabilityCause, IndicatorAvailability},
        plot::{AnySeries, PlotTooltip, candle::CandlePlot},
    },
};

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use data::chart::{PlotData, kline::KlineDataPoint};
use data::util::format_with_commas;
use exchange::{
    SizeUnit, Ticker, TickerInfo, Timeframe, Trade, UnixMs, adapter::MarketKind,
    unit::qty::volume_size_unit,
};

use iced::widget::{center, text};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// Resolution of the shared UTC-day trade books backing this indicator.
const ONE_MIN_MS: u64 = 60 * 1_000;
const FIVE_MIN_MS: u64 = 5 * ONE_MIN_MS;
// v3 invalidates v2 sidecars whose cached prefix could be merged a second time
// when another trade-history consumer requested an overlapping range.
const MINUTE_CACHE_SCHEMA_VERSION: u16 = 3;
/// UTC days of multi-venue trade history retained in the running sum. Days
/// are slimmed to compact minute deltas when available and truthful cached
/// five-minute deltas otherwise, so memory stays linear in time buckets.
pub(crate) const LOOKBACK_DAYS: usize = 90;
/// UTC days the chart actively backfills on this indicator's behalf.
///
/// Retention spans [`LOOKBACK_DAYS`], but eagerly pulling months of
/// multi-venue tick trades through rate-limited REST starves every other
/// fetch — including the chart's own kline requests, which is how candle
/// panes ended up blank while trade backfills ran for hours. Days beyond
/// this horizon therefore come from disk caches (shared with Footprint
/// History / Daily Delta and previous sessions) instead of eager fetching;
/// the running sum simply starts at the oldest day actually available.
pub(crate) const FETCH_LOOKBACK_DAYS: u16 = 7;
/// Upper bound for externally configured lookbacks (Daily Delta-style
/// settings), kept well inside the shared day books' retention limit.
const MAX_CONFIGURED_LOOKBACK_DAYS: u16 = 30;

#[derive(Debug, Deserialize, Serialize)]
struct CachedMinuteDeltas {
    schema_version: u16,
    source: String,
    day_start: u64,
    covered_through: u64,
    size_unit: SizeUnit,
    deltas: BTreeMap<u64, f64>,
}

#[derive(Debug)]
struct MinuteDeltaStage {
    source: Ticker,
    days: BTreeMap<u64, BTreeMap<u64, f64>>,
}

#[derive(Default)]
struct MinuteDeltaHistory {
    days: FxHashMap<(Ticker, u64), BTreeMap<u64, f64>>,
    checkpoints: FxHashMap<(Ticker, u64), Option<UnixMs>>,
    staging: FxHashMap<uuid::Uuid, MinuteDeltaStage>,
    cutoffs: FxHashMap<Ticker, UnixMs>,
}

impl MinuteDeltaHistory {
    fn oldest_retained_day(now: UnixMs) -> u64 {
        day_start(now)
            .saturating_sub(u64::from(FETCH_LOOKBACK_DAYS.saturating_sub(1)).saturating_mul(DAY_MS))
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
            "market_data/cvd-minutes/v{MINUTE_CACHE_SCHEMA_VERSION}/{unit}/{safe_identity}/{day}.cvdbin"
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

    fn read_cached_day(
        path: &Path,
        source: TickerInfo,
        day: u64,
    ) -> Option<(BTreeMap<u64, f64>, UnixMs)> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                log::warn!("Failed to read CVD minute cache {path:?}: {err}");
                return None;
            }
        };
        if !cache_has_completion_proof(path, &bytes) {
            log::warn!(
                "CVD minute cache {path:?} predates verified range proofs; preserving it on disk but refusing its checkpoint"
            );
            return None;
        }
        let (cached, _) = match bincode::serde::decode_from_slice::<CachedMinuteDeltas, _>(
            &bytes,
            bincode::config::standard(),
        ) {
            Ok(result) => result,
            Err(err) => {
                log::warn!("Ignoring corrupt CVD minute cache {path:?}: {err}");
                let _ = std::fs::remove_file(path);
                return None;
            }
        };
        let parsed_source = Ticker::parse_symbol_and_exchange(&cached.source);
        let day_end = day.saturating_add(DAY_MS);
        let deltas_are_valid = cached
            .deltas
            .iter()
            .all(|(minute, delta)| *minute >= day && *minute < day_end && delta.is_finite());
        if cached.schema_version != MINUTE_CACHE_SCHEMA_VERSION
            || !parsed_source.is_some_and(|ticker| ticker.same_market(&source.ticker))
            || cached.day_start != day
            || cached.covered_through < day
            || cached.covered_through >= day_end
            || cached.size_unit != volume_size_unit()
            || !deltas_are_valid
        {
            log::warn!("Ignoring mismatched CVD minute cache {path:?}");
            let _ = std::fs::remove_file(path);
            return None;
        }
        Some((cached.deltas, UnixMs::new(cached.covered_through)))
    }

    fn write_cached_day(
        path: &Path,
        source: TickerInfo,
        day: u64,
        covered_through: UnixMs,
        deltas: &BTreeMap<u64, f64>,
    ) -> std::io::Result<()> {
        static WRITE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((existing, existing_through)) = Self::read_cached_day(path, source, day)
            && (existing_through > covered_through
                || (existing_through == covered_through && existing.len() >= deltas.len()))
        {
            return Ok(());
        }
        let Some(parent) = path.parent() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "CVD minute cache path has no parent",
            ));
        };
        std::fs::create_dir_all(parent)?;
        let payload = CachedMinuteDeltas {
            schema_version: MINUTE_CACHE_SCHEMA_VERSION,
            source: source.ticker.symbol_and_exchange_string(),
            day_start: day,
            covered_through: covered_through.as_u64(),
            size_unit: volume_size_unit(),
            deltas: deltas.clone(),
        };
        let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard())
            .map_err(std::io::Error::other)?;
        write_cache_with_completion_proof(path, &bytes)
    }

    /// Returns the checkpoint and newly loaded deltas. Repeated planner ticks
    /// return the checkpoint without re-merging the same minute values.
    fn load_day(
        &mut self,
        source: TickerInfo,
        day: u64,
    ) -> Option<(UnixMs, Option<BTreeMap<u64, f64>>)> {
        if let Some(checkpoint) = self.checkpoints.get(&(source.ticker, day)) {
            return checkpoint.map(|through| (through, None));
        }
        let path = Self::cache_path(source, day);
        let Some((deltas, covered_through)) = Self::read_cached_day(&path, source, day) else {
            self.checkpoints.insert((source.ticker, day), None);
            return None;
        };
        self.days.insert((source.ticker, day), deltas.clone());
        self.checkpoints
            .insert((source.ticker, day), Some(covered_through));
        Some((covered_through, Some(deltas)))
    }

    fn prepare(&mut self, source: TickerInfo, cutoff: UnixMs) {
        // The first planner pass can run before the trade WebSocket connects,
        // making this a provisional boundary. Once the first live execution
        // is known, the planner advances `cutoff` to immediately before it so
        // history can fill the startup seam without overlapping live data.
        self.cutoffs
            .entry(source.ticker)
            .and_modify(|current| *current = (*current).max(cutoff))
            .or_insert(cutoff);
    }

    fn stage(&mut self, req_id: uuid::Uuid, source: TickerInfo, trades: &[Trade]) {
        let cutoff = self.cutoffs.get(&source.ticker).copied();
        let oldest = Self::oldest_retained_day(UnixMs::now());
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || source.market_type() == MarketKind::InversePerps;
        if self
            .staging
            .get(&req_id)
            .is_some_and(|stage| !stage.source.same_market(&source.ticker))
        {
            return;
        }

        // Build the page outside `self.staging` so the checkpoint map remains
        // available while filtering. The shared planner can legitimately fetch
        // a wider range when another indicator has less cache coverage; CVD
        // must not add the already-persisted prefix into its minute map again.
        let mut accepted = BTreeMap::<u64, BTreeMap<u64, f64>>::new();
        for trade in trades {
            let day = day_start(trade.time);
            let already_covered = self
                .checkpoints
                .get(&(source.ticker, day))
                .copied()
                .flatten()
                .is_some_and(|checkpoint| trade.time <= checkpoint);
            if trade.time.as_u64() < oldest
                || cutoff.is_some_and(|boundary| trade.time > boundary)
                || already_covered
            {
                continue;
            }
            let minute = trade.time.as_u64() / ONE_MIN_MS * ONE_MIN_MS;
            let notional = trade_notional(*trade, qty_is_quote);
            let signed = if trade.is_sell { -notional } else { notional };
            *accepted.entry(day).or_default().entry(minute).or_default() += signed;
        }

        let stage = self
            .staging
            .entry(req_id)
            .or_insert_with(|| MinuteDeltaStage {
                source: source.ticker,
                days: BTreeMap::new(),
            });
        if !stage.source.same_market(&source.ticker) {
            return;
        }
        for (day, deltas) in accepted {
            let target = stage.days.entry(day).or_default();
            for (minute, delta) in deltas {
                *target.entry(minute).or_default() += delta;
            }
        }
    }

    fn commit(&mut self, req_id: uuid::Uuid) -> Vec<(Ticker, u64, BTreeMap<u64, f64>)> {
        let Some(stage) = self.staging.remove(&req_id) else {
            return Vec::new();
        };
        let mut committed = Vec::with_capacity(stage.days.len());
        for (day, deltas) in stage.days {
            let target = self.days.entry((stage.source, day)).or_default();
            for (minute, delta) in &deltas {
                *target.entry(*minute).or_default() += delta;
            }
            committed.push((stage.source, day, deltas));
        }
        committed
    }

    fn discard(&mut self, req_id: uuid::Uuid) {
        self.staging.remove(&req_id);
    }

    fn persist(&mut self, source: TickerInfo, day: u64, covered_through: UnixMs) {
        let Some(deltas) = self.days.get(&(source.ticker, day)).cloned() else {
            return;
        };
        self.checkpoints
            .insert((source.ticker, day), Some(covered_through));
        let path = Self::cache_path(source, day);
        std::thread::Builder::new()
            .name("cvd-minute-cache-writer".to_string())
            .spawn(move || {
                if let Err(err) =
                    Self::write_cached_day(&path, source, day, covered_through, &deltas)
                {
                    log::warn!("Failed to write CVD minute cache {path:?}: {err}");
                }
            })
            .map(|_| ())
            .unwrap_or_else(|err| log::warn!("Failed to spawn CVD minute cache writer: {err}"));
    }

    fn clear(&mut self) {
        self.days.clear();
        self.checkpoints.clear();
        self.staging.clear();
        self.cutoffs.clear();
    }
}

/// One aggregated CVD bar. Values are USD notional (buy minus sell), matching
/// the shared day books used by Footprint History and Daily Delta.
#[derive(Debug, Clone, Copy)]
pub struct DeltaCandle {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    /// Buy - sell notional traded inside this bucket.
    delta: f64,
    /// Venue membership for this continuous coverage segment.
    source_mask: u128,
}

fn change_text(candle: &DeltaCandle) -> String {
    let sign = if candle.delta >= 0.0 { "+" } else { "-" };
    format!("Change: {sign}${}", format_with_commas(candle.delta.abs()))
}

pub struct CumulativeDeltaIndicator {
    cache: Caches,
    inner: FootprintHistoryIndicator,
    minute_history: MinuteDeltaHistory,
    lookback_days: usize,
    /// Chart bucket width in milliseconds. `None` on tick basis, where
    /// time-keyed aggregate candles cannot be placed.
    interval_ms: Option<u64>,
    merged: MergedDays,
    candles: BTreeMap<UnixMs, DeltaCandle>,
    has_data: bool,
}

impl CumulativeDeltaIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(LOOKBACK_DAYS as u16);
        inner.set_day_book_retain(DayBookRetain::CVD_DELTAS_ONLY);
        Self {
            cache: Caches::default(),
            inner,
            minute_history: MinuteDeltaHistory::default(),
            lookback_days: LOOKBACK_DAYS,
            interval_ms: None,
            merged: MergedDays::new(LOOKBACK_DAYS),
            candles: BTreeMap::new(),
            has_data: false,
        }
    }

    fn availability_for(basis: Basis) -> IndicatorAvailability {
        match basis {
            Basis::Tick(_) => IndicatorAvailability::Unavailable(AvailabilityCause::Basis(basis)),
            Basis::Time(timeframe) => {
                // The compact day book resolves one-minute deltas. Sub-minute
                // buckets cannot be filled honestly without retaining raw tape.
                if timeframe < Timeframe::M1 {
                    IndicatorAvailability::Unavailable(AvailabilityCause::Timeframe(timeframe))
                } else {
                    IndicatorAvailability::Available
                }
            }
        }
    }

    fn indicator_elem<'a>(
        &'a self,
        main_chart: &'a ViewState,
        data_labels_always_visible: bool,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        if let Some(message) = self.unavailable_message(main_chart, "CVD") {
            return center(text(message)).into();
        }

        let tooltip = |candle: &DeltaCandle, _next: Option<&DeltaCandle>| {
            PlotTooltip::new(change_text(candle))
        };

        let plot = CandlePlot::new(
            |candle: &DeltaCandle| candle.open as f32,
            |candle: &DeltaCandle| candle.high as f32,
            |candle: &DeltaCandle| candle.low as f32,
            |candle: &DeltaCandle| candle.close as f32,
        )
        .with_tooltip(tooltip);

        indicator_row(
            main_chart,
            &self.cache,
            data_labels_always_visible,
            plot,
            AnySeries::forward_unix_ms(&self.candles),
            visible_range,
        )
    }

    fn refresh_merged(&mut self) {
        let had_data = self.has_data;
        if let Some(rebuild_from) = self.merged.refresh(&self.inner, self.lookback_days) {
            self.assemble_candles_from(rebuild_from);
        }
        if !had_data && self.has_data {
            log::debug!(
                "CVD published {} verified candle(s) across partial/complete history",
                self.candles.len()
            );
        }
    }

    /// Fold the cached per-day venue-merged deltas into running CVD candles.
    fn assemble_candles(&mut self) {
        let Some(first_day) = self.merged.days.first().map(|day| day.day_ts) else {
            self.candles.clear();
            self.has_data = false;
            return;
        };
        self.candles.clear();
        self.assemble_candles_from(first_day);
    }

    /// Rebuild only the affected suffix. Live trade batches normally dirty a
    /// single minute in the current day, so this keeps M1/M3 CVD work bounded
    /// instead of rescanning the 90-day retention window on every update.
    fn assemble_candles_from(&mut self, rebuild_from: u64) {
        let Some(interval_ms) = self.interval_ms.filter(|ms| *ms >= ONE_MIN_MS) else {
            self.candles.clear();
            self.has_data = false;
            return;
        };

        let rebuild_from = rebuild_from / interval_ms * interval_ms;
        let rebuild_key = UnixMs::new(rebuild_from);
        let previous = self
            .candles
            .range(..rebuild_key)
            .next_back()
            .map(|(bucket, candle)| (*bucket, *candle));
        let mut cumulative = previous.map_or(0.0, |(_, candle)| candle.close);
        let mut last_source_mask = previous.map(|(_, candle)| candle.source_mask);
        let mut last_bucket = previous.map(|(bucket, _)| bucket.as_u64());
        drop(self.candles.split_off(&rebuild_key));

        for day in &self.merged.days {
            if day.day_ts.saturating_add(DAY_MS) <= rebuild_from {
                continue;
            }
            let buckets = day
                .sources
                .iter()
                .flat_map(|source| {
                    source
                        .deltas
                        .range(rebuild_from..)
                        .map(|(bucket, _)| *bucket)
                })
                .map(|bucket| bucket / interval_ms * interval_ms)
                .collect::<BTreeSet<_>>();
            for bucket in buckets {
                let bucket_end = bucket.saturating_add(interval_ms).saturating_sub(1);
                let mut source_mask = 0_u128;
                let mut source_count = 0_u16;
                let mut bucket_deltas = BTreeMap::<u64, f64>::new();
                for (index, source) in day.sources.iter().enumerate() {
                    let source_is_covered = !source
                        .missing_ranges
                        .iter()
                        .any(|(start, end)| bucket <= *end && bucket_end >= *start);
                    if !source_is_covered {
                        continue;
                    }
                    if index < u128::BITS as usize {
                        source_mask |= 1_u128 << index;
                    }
                    source_count = source_count.saturating_add(1);
                    for (time, delta) in source.deltas.range(bucket..=bucket_end) {
                        *bucket_deltas.entry(*time).or_default() += *delta;
                    }
                }
                bucket_deltas.retain(|_, delta| delta.abs() > DELTA_ROUNDING_EPSILON);
                if source_count == 0 || bucket_deltas.is_empty() {
                    continue;
                }

                let crossed_gap = last_bucket.is_some_and(|previous_bucket| {
                    let between_start = previous_bucket.saturating_add(interval_ms);
                    let between_end = bucket.saturating_sub(1);
                    between_start <= between_end
                        && self.merged.sources_have_gap_between(
                            last_source_mask.unwrap_or_default() & source_mask,
                            between_start,
                            between_end,
                        )
                });
                if last_source_mask.is_some_and(|previous| previous != source_mask) || crossed_gap {
                    cumulative = 0.0;
                }
                let open = cumulative;
                let mut high = open;
                let mut low = open;
                let mut delta = 0.0;
                for step in bucket_deltas.values() {
                    cumulative += step;
                    delta += step;
                    high = high.max(cumulative);
                    low = low.min(cumulative);
                }
                self.candles.insert(
                    UnixMs::new(bucket),
                    DeltaCandle {
                        open,
                        high,
                        low,
                        close: cumulative,
                        delta,
                        source_mask,
                    },
                );
                last_source_mask = Some(source_mask);
                last_bucket = Some(bucket);
            }
        }
        self.has_data = !self.candles.is_empty();
        self.cache.clear_all();
    }
}

impl KlineIndicatorImpl for CumulativeDeltaIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
    }

    fn clear_crosshair_caches(&mut self) {
        self.cache.clear_crosshair();
    }

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        data_labels_always_visible: bool,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        self.indicator_elem(chart, data_labels_always_visible, visible_range)
    }

    fn availability(&self, chart: &ViewState) -> IndicatorAvailability {
        match Self::availability_for(chart.basis) {
            IndicatorAvailability::Available if !self.has_data => IndicatorAvailability::Unknown,
            available => available,
        }
    }

    fn rebuild_from_source(&mut self, source: &PlotData<KlineDataPoint>) {
        self.interval_ms = match source {
            PlotData::TimeBased(series) => Some(series.interval.to_milliseconds()),
            PlotData::TickBased(_) => None,
        };
        self.assemble_candles();
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn on_basis_change(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild_from_source(source);
    }

    fn configure_footprint_history(&mut self, sources: &[TickerInfo], aggregate: bool) {
        self.inner.configure_footprint_history(sources, aggregate);
        self.inner.set_lookback_days(self.lookback_days as u16);
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn set_trade_history_lookback(&mut self, days: u16) {
        let days = days.clamp(1, MAX_CONFIGURED_LOOKBACK_DAYS);
        self.lookback_days = usize::from(days);
        self.inner.set_lookback_days(days);
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn reset_trade_history_backfill(&mut self) {
        self.inner.reset_trade_history_backfill();
        self.minute_history.clear();
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn prepare_footprint_history(&mut self, source: TickerInfo, cutoff: UnixMs) {
        self.inner.prepare_footprint_history(source, cutoff);
        self.minute_history.prepare(source, cutoff);
    }

    fn load_cached_footprint_day(&mut self, source: TickerInfo, day: UnixMs) -> Option<UnixMs> {
        if day.as_u64() < MinuteDeltaHistory::oldest_retained_day(UnixMs::now()) {
            return Some(UnixMs::new(
                day.as_u64().saturating_add(DAY_MS).saturating_sub(1),
            ));
        }
        // Load the shared v7 book as an immediate coarse fallback, but only the
        // compact minute sidecar satisfies an M1/M3 CVD history request.
        let shared = self.inner.load_cached_footprint_day(source, day);
        if let Some(shared_through) = shared
            && FootprintHistoryIndicator::day_is_complete(day, shared_through)
        {
            self.inner.slim_day_for_cvd(day.as_u64());
        }
        let minute = self.minute_history.load_day(source, day.as_u64());
        if let Some((_, Some(deltas))) = minute.as_ref() {
            self.inner
                .merge_exact_cvd_minutes(source.ticker, day.as_u64(), deltas);
        }
        if shared.is_some() || minute.is_some() {
            self.merged.mark_dirty_from(day_start(day));
            self.refresh_merged();
        }
        minute.map(|(covered_through, _)| covered_through)
    }

    fn persist_cached_footprint_day(
        &mut self,
        source: TickerInfo,
        day_start: UnixMs,
        covered_through: UnixMs,
    ) {
        // The compact CVD consumer does not write the shared price-level book,
        // but its inner merger still needs the same verified range checkpoint
        // before it may publish an aggregate day.
        self.inner
            .persist_day_cache(source, day_start, covered_through);
        self.minute_history
            .persist(source, day_start.as_u64(), covered_through);
        self.merged.mark_dirty_from(day_start.as_u64());
        self.refresh_merged();
    }

    fn on_source_trades(&mut self, source: TickerInfo, trades: &[Trade], historical: bool) {
        self.inner.on_source_trades(source, trades, historical);
        if let Some(earliest) = trades.iter().map(|trade| trade.time.as_u64()).min() {
            // Rebuild the containing native five-minute bucket. The coarse
            // cache entry includes this minute and must be replaced alongside
            // its exact minute deltas to avoid counting a live execution twice.
            self.merged
                .mark_dirty_from(earliest / FIVE_MIN_MS * FIVE_MIN_MS);
            self.refresh_merged();
        }
    }

    fn stage_source_trades(&mut self, req_id: uuid::Uuid, source: TickerInfo, trades: &[Trade]) {
        self.minute_history.stage(req_id, source, trades);
    }

    fn commit_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        let committed = self.minute_history.commit(req_id);
        let source = committed.first().map(|(source, _, _)| *source);
        let committed_days = committed.len();
        let committed_minutes = committed
            .iter()
            .map(|(_, _, deltas)| deltas.len())
            .sum::<usize>();
        let earliest_minute = committed
            .iter()
            .flat_map(|(_, _, deltas)| deltas.keys())
            .min()
            .copied();
        let latest_minute = committed
            .iter()
            .flat_map(|(_, _, deltas)| deltas.keys())
            .max()
            .copied();
        let mut dirty_from = None::<u64>;
        for (source, day, deltas) in committed {
            dirty_from = Some(dirty_from.map_or(day, |current| current.min(day)));
            self.inner.merge_exact_cvd_minutes(source, day, &deltas);
        }
        if let Some(dirty_from) = dirty_from {
            self.merged.mark_dirty_from(dirty_from);
            self.refresh_merged();
        }
        log::debug!(
            "CVD history commit req={req_id} source={} days={committed_days} minute_buckets={committed_minutes} first={:?} last={:?} published_candles={}",
            source.map_or_else(|| "none".to_string(), |source| source.to_string()),
            earliest_minute,
            latest_minute,
            self.candles.len(),
        );
    }

    fn mark_incomplete_trade_history(
        &mut self,
        source: TickerInfo,
        missing_ranges: &[(UnixMs, UnixMs)],
    ) {
        self.inner
            .mark_incomplete_trade_history(source, missing_ranges);
        self.merged.invalidate_all();
        self.refresh_merged();
        log::debug!(
            "CVD retained verified history for {} with {} exact missing range(s); candles={}",
            source.ticker,
            missing_ranges.len(),
            self.candles.len()
        );
    }

    fn discard_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.minute_history.discard(req_id);
    }
}

struct MergedDay {
    day_ts: u64,
    sources: Vec<CvdSourceDay>,
}

/// Per-day cache of venue-merged one-minute deltas, so panning/zooming and
/// live ticks never re-absorb whole days of trades per frame.
struct MergedDays {
    generation: u64,
    built: Option<(u64, usize, u64)>,
    dirty_from_day: Option<u64>,
    days: Vec<MergedDay>,
}

impl MergedDays {
    fn new(_lookback_days: usize) -> Self {
        Self {
            generation: 0,
            built: None,
            dirty_from_day: None,
            days: Vec::new(),
        }
    }

    fn invalidate_all(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn mark_dirty_from(&mut self, day_ts: u64) {
        let day_ts = day_ts / ONE_MIN_MS * ONE_MIN_MS;
        self.dirty_from_day = Some(
            self.dirty_from_day
                .map_or(day_ts, |current| current.min(day_ts)),
        );
    }

    /// Refresh merged source buckets and return the earliest timestamp whose
    /// running CVD candles must be rebuilt.
    fn refresh(&mut self, inner: &FootprintHistoryIndicator, lookback_days: usize) -> Option<u64> {
        let today = day_start(UnixMs::now());
        let lookback_days = lookback_days.max(1);
        let needs_full_rebuild = self.built.is_none_or(|built| {
            let (generation, days, built_today) = built;
            generation != self.generation || days != lookback_days || built_today != today
        });
        if needs_full_rebuild {
            self.built = Some((self.generation, lookback_days, today));
            self.dirty_from_day = None;
            self.days = (0..lookback_days)
                .rev()
                .map(|offset| {
                    let day_ts = today.saturating_sub(offset as u64 * DAY_MS);
                    MergedDay {
                        day_ts,
                        sources: inner.cvd_source_days_from(day_ts, day_ts),
                    }
                })
                .collect();
            self.days.first().map(|day| day.day_ts)
        } else if let Some(from) = self.dirty_from_day.take() {
            let from_day = day_start(UnixMs::new(from));
            for day in &mut self.days {
                if day.day_ts < from_day {
                    continue;
                }
                day.sources = inner.cvd_source_days_from(day.day_ts, day.day_ts);
            }
            Some(from)
        } else {
            None
        }
    }

    fn sources_have_gap_between(&self, source_mask: u128, from: u64, to: u64) -> bool {
        if source_mask == 0 || from > to {
            return false;
        }
        self.days.iter().any(|day| {
            if day.day_ts > to || day.day_ts.saturating_add(DAY_MS) <= from {
                return false;
            }
            day.sources.iter().enumerate().any(|(index, source)| {
                index < u128::BITS as usize
                    && source_mask & (1_u128 << index) != 0
                    && source
                        .missing_ranges
                        .iter()
                        .any(|(start, end)| from <= *end && to >= *start)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, adapter::Exchange, unit::Price, unit::Qty};

    const FIVE_MIN: u64 = 5 * 60 * 1_000;
    const FIFTEEN_MIN: u64 = 15 * 60 * 1_000;

    fn source(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    #[test]
    fn tooltip_change_uses_the_candle_bucket_delta() {
        let rising = DeltaCandle {
            open: 100.0,
            high: 175.0,
            low: 90.0,
            close: 160.0,
            delta: 60.0,
            source_mask: 1,
        };
        let falling = DeltaCandle {
            delta: -25.0,
            ..rising
        };

        assert_eq!(change_text(&rising), "Change: +$60.00");
        assert_eq!(change_text(&falling), "Change: -$25.00");
    }

    fn accept_verified_day(
        indicator: &mut CumulativeDeltaIndicator,
        source: TickerInfo,
        day: u64,
        through: UnixMs,
    ) {
        indicator
            .inner
            .accept_verified_day(source, UnixMs::new(day), through);
        indicator.merged.mark_dirty_from(day);
        indicator.refresh_merged();
    }

    #[test]
    fn aggregates_venues_into_running_cvd_candles() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.interval_ms = Some(FIFTEEN_MIN);

        let today = day_start(UnixMs::now());
        // Buy and sell sit in different 5m day-book buckets within the first
        // M15 bucket, so the candle records the intra-bucket high before
        // fading.
        indicator.on_source_trades(binance, &[trade(today + 60_000, 10.0, 100.0, false)], true);
        indicator.on_source_trades(
            bybit,
            &[trade(today + FIVE_MIN + 30_000, 10.0, 50.0, true)],
            true,
        );
        // Next M15 bucket, next day-book bucket.
        indicator.on_source_trades(
            binance,
            &[trade(
                today + 2 * FIVE_MIN + 30_000 + FIFTEEN_MIN,
                10.0,
                200.0,
                false,
            )],
            true,
        );

        assert!(indicator.has_data);
        let first = indicator.candles[&UnixMs::new(today)];
        assert_eq!(first.open, 0.0);
        assert_eq!(first.high, 1_000.0);
        assert_eq!(first.low, 0.0);
        assert_eq!(first.close, 500.0);
        assert_eq!(first.delta, 500.0);

        let second = indicator.candles[&UnixMs::new(today + FIFTEEN_MIN)];
        assert_eq!(second.open, 500.0);
        assert_eq!(second.high, 2_500.0);
        assert_eq!(second.low, 500.0);
        assert_eq!(second.close, 2_500.0);
        assert_eq!(second.delta, 2_000.0);
        assert_eq!(indicator.candles.len(), 2);
    }

    #[test]
    fn single_source_cvd_still_builds_without_aggregation() {
        let only = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(Timeframe::M5.to_milliseconds());

        let today = day_start(UnixMs::now());
        indicator.on_source_trades(only, &[trade(today + 10_000, 5.0, 2.0, true)], true);

        let first = indicator.candles[&UnixMs::new(today)];
        assert_eq!(first.close, -10.0);
        assert_eq!(first.delta, -10.0);
    }

    #[test]
    fn one_minute_cvd_uses_execution_derived_minute_closes() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(Timeframe::M1.to_milliseconds());

        let today = day_start(UnixMs::now());
        indicator.on_source_trades(
            only,
            &[
                trade(today + 10_000, 10.0, 100.0, false),
                trade(today + 70_000, 10.0, 40.0, true),
                trade(today + 130_000, 10.0, 20.0, false),
            ],
            true,
        );

        let first = indicator.candles[&UnixMs::new(today)];
        assert_eq!(first.open, 0.0);
        assert_eq!(first.close, 1_000.0);
        assert_eq!(first.delta, 1_000.0);
        let second = indicator.candles[&UnixMs::new(today + ONE_MIN_MS)];
        assert_eq!(second.open, 1_000.0);
        assert_eq!(second.close, 600.0);
        assert_eq!(second.delta, -400.0);
        let third = indicator.candles[&UnixMs::new(today + 2 * ONE_MIN_MS)];
        assert_eq!(third.open, 600.0);
        assert_eq!(third.close, 800.0);
        assert_eq!(third.delta, 200.0);
    }

    #[test]
    fn streamed_history_commits_every_cvd_minute_without_price_books() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(Timeframe::M1.to_milliseconds());

        let today = day_start(UnixMs::now());
        let req_id = uuid::Uuid::new_v4();
        indicator.prepare_footprint_history(only, UnixMs::new(today + DAY_MS - 1));
        indicator.stage_source_trades(
            req_id,
            only,
            &[
                trade(today + 10_000, 10.0, 100.0, false),
                trade(today + 70_000, 10.0, 40.0, true),
                trade(today + 130_000, 10.0, 20.0, false),
            ],
        );
        indicator.commit_staged_source_trades(req_id);
        accept_verified_day(&mut indicator, only, today, UnixMs::new(today + DAY_MS - 1));

        assert_eq!(indicator.candles.len(), 3);
        assert_eq!(indicator.candles[&UnixMs::new(today)].close, 1_000.0);
        assert_eq!(
            indicator.candles[&UnixMs::new(today + ONE_MIN_MS)].close,
            600.0
        );
        assert_eq!(
            indicator.candles[&UnixMs::new(today + 2 * ONE_MIN_MS)].close,
            800.0
        );
    }

    #[test]
    fn cached_minute_prefix_is_not_merged_twice_by_an_overlapping_fetch() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let today = day_start(UnixMs::now());
        let checkpoint = UnixMs::new(today + ONE_MIN_MS - 1);
        let cached_delta = 750.0;
        let mut history = MinuteDeltaHistory::default();
        history.days.insert(
            (only.ticker, today),
            BTreeMap::from([(today, cached_delta)]),
        );
        history
            .checkpoints
            .insert((only.ticker, today), Some(checkpoint));
        history.prepare(only, UnixMs::new(today + 3 * ONE_MIN_MS));

        let covered_trade = trade(today + 10_000, 10.0, 100.0, false);
        let new_trade = trade(today + ONE_MIN_MS + 10_000, 10.0, 20.0, false);
        let req_id = uuid::Uuid::new_v4();
        history.stage(req_id, only, &[covered_trade, new_trade]);
        let committed = history.commit(req_id);

        assert_eq!(history.days[&(only.ticker, today)][&today], cached_delta);
        assert_eq!(committed.len(), 1);
        assert!(!committed[0].2.contains_key(&today));
        assert!(committed[0].2.contains_key(&(today + ONE_MIN_MS)));
    }

    #[test]
    fn legacy_v2_minute_sidecar_is_rejected() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let day = day_start(UnixMs::now());
        let root =
            std::env::temp_dir().join(format!("flowsurface-cvd-v2-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create cache test directory");
        let path = root.join("day.cvdbin");
        let payload = CachedMinuteDeltas {
            schema_version: 2,
            source: only.ticker.symbol_and_exchange_string(),
            day_start: day,
            covered_through: day + ONE_MIN_MS - 1,
            size_unit: volume_size_unit(),
            deltas: BTreeMap::from([(day, 1_000.0)]),
        };
        let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard())
            .expect("encode legacy cache");
        write_cache_with_completion_proof(&path, &bytes).expect("write legacy cache");

        assert!(MinuteDeltaHistory::read_cached_day(&path, only, day).is_none());
        std::fs::remove_dir_all(&root).expect("remove cache test directory");
    }

    #[test]
    fn delayed_live_start_backfills_the_provisional_cutoff_gap() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(Timeframe::M1.to_milliseconds());

        let today = day_start(UnixMs::now());
        let provisional_cutoff = UnixMs::new(today + 10_000);
        let first_live = UnixMs::new(today + 2 * ONE_MIN_MS + 10_000);
        indicator.prepare_footprint_history(only, provisional_cutoff);
        indicator.on_source_trades(
            only,
            &[trade(first_live.as_u64(), 10.0, 20.0, false)],
            false,
        );

        // Once the live seam is known, the planner requests the interval
        // between its provisional cutoff and the first live execution.
        indicator.prepare_footprint_history(only, first_live.saturating_sub(1));
        let req_id = uuid::Uuid::new_v4();
        indicator.stage_source_trades(
            req_id,
            only,
            &[trade(today + ONE_MIN_MS + 10_000, 10.0, 10.0, false)],
        );
        indicator.commit_staged_source_trades(req_id);
        accept_verified_day(&mut indicator, only, today, first_live.saturating_sub(1));

        assert_eq!(indicator.candles.len(), 2);
        assert_eq!(
            indicator.candles[&UnixMs::new(today + ONE_MIN_MS)].close,
            100.0
        );
        assert_eq!(
            indicator.candles[&UnixMs::new(today + 2 * ONE_MIN_MS)].close,
            300.0
        );
    }

    #[test]
    fn minute_sidecar_round_trips_exact_deltas_and_coverage() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let day = day_start(UnixMs::now());
        let covered_through = UnixMs::new(day + 3 * ONE_MIN_MS - 1);
        let root = std::env::temp_dir().join(format!(
            "flowsurface-cvd-minute-cache-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create cache test directory");
        let path = root.join("day.cvdbin");
        let deltas = BTreeMap::from([
            (day, 1_000.0),
            (day + ONE_MIN_MS, -400.0),
            (day + 2 * ONE_MIN_MS, 200.0),
        ]);

        MinuteDeltaHistory::write_cached_day(&path, only, day, covered_through, &deltas)
            .expect("write minute cache");
        let (loaded, loaded_through) =
            MinuteDeltaHistory::read_cached_day(&path, only, day).expect("read minute cache");

        assert_eq!(loaded, deltas);
        assert_eq!(loaded_through, covered_through);
        std::fs::remove_dir_all(&root).expect("remove cache test directory");
    }

    #[test]
    fn partial_verified_history_keeps_segments_and_restarts_after_gap() {
        let only = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(ONE_MIN_MS);

        let today = day_start(UnixMs::now());
        indicator.prepare_footprint_history(only, UnixMs::new(today + 6 * ONE_MIN_MS));
        let req_id = uuid::Uuid::new_v4();
        indicator.stage_source_trades(
            req_id,
            only,
            &[
                trade(today + ONE_MIN_MS + 1_000, 10.0, 10.0, false),
                trade(today + 5 * ONE_MIN_MS + 1_000, 10.0, 20.0, false),
            ],
        );
        indicator.commit_staged_source_trades(req_id);
        indicator.inner.accept_verified_day(
            only,
            UnixMs::new(today),
            UnixMs::new(today + 2 * ONE_MIN_MS - 1),
        );
        indicator.mark_incomplete_trade_history(
            only,
            &[(
                UnixMs::new(today + 2 * ONE_MIN_MS),
                UnixMs::new(today + 4 * ONE_MIN_MS - 1),
            )],
        );

        assert_eq!(indicator.candles.len(), 2);
        assert_eq!(
            indicator.candles[&UnixMs::new(today + ONE_MIN_MS)].close,
            100.0
        );
        let resumed = indicator.candles[&UnixMs::new(today + 5 * ONE_MIN_MS)];
        assert_eq!(resumed.open, 0.0);
        assert_eq!(resumed.close, 200.0);
    }

    #[test]
    fn recorder_finality_suffix_does_not_gap_the_live_seam_bucket() {
        let only = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(FIVE_MIN);

        let today = day_start(UnixMs::now());
        let live_start = UnixMs::new(today + 5 * ONE_MIN_MS + 10_000);
        let live_cutoff = live_start.saturating_sub(1);
        indicator.prepare_footprint_history(only, live_cutoff);

        let req_id = uuid::Uuid::new_v4();
        indicator.stage_source_trades(
            req_id,
            only,
            &[trade(today + ONE_MIN_MS, 10.0, 10.0, false)],
        );
        indicator.commit_staged_source_trades(req_id);
        accept_verified_day(&mut indicator, only, today, live_cutoff);

        indicator.mark_incomplete_trade_history(
            only,
            &[(live_start, UnixMs::new(today + 10 * ONE_MIN_MS - 1))],
        );
        indicator.on_source_trades(
            only,
            &[trade(live_start.as_u64(), 10.0, 20.0, false)],
            false,
        );
        // The same stale finality response can also arrive after the first
        // live batch. Neither ordering may poison the seam bucket.
        indicator.mark_incomplete_trade_history(
            only,
            &[(live_start, UnixMs::new(today + 10 * ONE_MIN_MS - 1))],
        );

        assert_eq!(indicator.candles.len(), 2);
        assert_eq!(indicator.candles[&UnixMs::new(today)].close, 100.0);
        let seam = indicator.candles[&UnixMs::new(today + 5 * ONE_MIN_MS)];
        assert_eq!(seam.open, 100.0);
        assert_eq!(seam.close, 300.0);
    }

    #[test]
    fn aggregate_cvd_keeps_proven_venues_when_one_source_starts_late() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let sources = [binance, bybit, hyperliquid];
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&sources, true);
        indicator.interval_ms = Some(FIVE_MIN);

        let today = day_start(UnixMs::now());
        let cutoff = UnixMs::new(today + 20 * ONE_MIN_MS);
        for (source, first_qty, second_qty) in [
            (binance, Some(10.0), 20.0),
            (bybit, Some(5.0), 10.0),
            (hyperliquid, None, 7.0),
        ] {
            indicator.prepare_footprint_history(source, cutoff);
            let mut trades = vec![trade(today + 11 * ONE_MIN_MS, 10.0, second_qty, false)];
            if let Some(first_qty) = first_qty {
                trades.push(trade(today + ONE_MIN_MS, 10.0, first_qty, false));
            }
            let req_id = uuid::Uuid::new_v4();
            indicator.stage_source_trades(req_id, source, &trades);
            indicator.commit_staged_source_trades(req_id);
        }
        for source in [binance, bybit] {
            accept_verified_day(&mut indicator, source, today, cutoff);
        }
        indicator.mark_incomplete_trade_history(
            hyperliquid,
            &[(UnixMs::new(today), UnixMs::new(today + 10 * ONE_MIN_MS - 1))],
        );

        let partial = indicator.candles[&UnixMs::new(today)];
        assert_eq!(partial.open, 0.0);
        assert_eq!(partial.close, 150.0);
        assert_eq!(partial.source_mask.count_ones(), 2);

        let complete = indicator.candles[&UnixMs::new(today + 10 * ONE_MIN_MS)];
        assert_eq!(complete.open, 0.0);
        assert_eq!(complete.close, 370.0);
        assert_eq!(complete.source_mask.count_ones(), 3);
    }

    #[test]
    fn aggregate_cvd_keeps_current_verified_segments_when_older_days_are_uncovered() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let sources = [binance, bybit, hyperliquid];
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&sources, true);
        indicator.interval_ms = Some(FIVE_MIN);

        let today = day_start(UnixMs::now());
        let cutoff = UnixMs::new(today + 30 * ONE_MIN_MS);
        for source in sources {
            indicator.prepare_footprint_history(source, cutoff);
            let req_id = uuid::Uuid::new_v4();
            indicator.stage_source_trades(
                req_id,
                source,
                &[
                    trade(today + ONE_MIN_MS, 10.0, 10.0, false),
                    trade(today + 11 * ONE_MIN_MS, 10.0, 20.0, false),
                ],
            );
            indicator.commit_staged_source_trades(req_id);
        }

        accept_verified_day(&mut indicator, binance, today, cutoff);
        for source in [bybit, hyperliquid] {
            indicator.mark_incomplete_trade_history(
                source,
                &[
                    (
                        UnixMs::new(today + 5 * ONE_MIN_MS),
                        UnixMs::new(today + 6 * ONE_MIN_MS - 1),
                    ),
                    (
                        UnixMs::new(today + 29 * ONE_MIN_MS),
                        UnixMs::new(today + 30 * ONE_MIN_MS),
                    ),
                ],
            );
        }

        // Five older lookback days predate the recorder completely. Recording
        // those exact gaps must not erase today's already-proven segments.
        for offset in 1..=5 {
            let old_day = today - offset * DAY_MS;
            for source in sources {
                indicator.mark_incomplete_trade_history(
                    source,
                    &[(UnixMs::new(old_day), UnixMs::new(old_day + DAY_MS - 1))],
                );
            }
        }

        assert_eq!(indicator.candles.len(), 2);
        assert_eq!(indicator.candles[&UnixMs::new(today)].close, 300.0);
        let resumed = indicator.candles[&UnixMs::new(today + 10 * ONE_MIN_MS)];
        assert_eq!(resumed.open, 0.0);
        assert_eq!(resumed.close, 600.0);
    }

    #[test]
    fn one_minute_and_higher_are_available_but_tick_basis_is_not() {
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::M1)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::M3)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::M5)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::H4)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::MS1000)),
            IndicatorAvailability::Unavailable(AvailabilityCause::Timeframe(_))
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Tick(data::aggr::TickCount(100))),
            IndicatorAvailability::Unavailable(AvailabilityCause::Basis(_))
        ));
    }
}
