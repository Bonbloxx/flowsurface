use super::KlineIndicatorImpl;
use super::footprint_history::{
    DAY_MS, DayBookRetain, FootprintHistoryIndicator, day_start, trade_notional,
};
use crate::chart::{
    Basis, Caches, Message, ViewState,
    indicator::{
        indicator_row,
        kline::{AvailabilityCause, IndicatorAvailability},
        plot::{AnySeries, PlotTooltip, candle::CandlePlot},
    },
};

use std::collections::BTreeMap;
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
// v2 invalidates sidecars that could mark the provisional-cutoff/live-start
// interval covered even though the old staging boundary discarded its trades.
const MINUTE_CACHE_SCHEMA_VERSION: u16 = 2;
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
        for trade in trades {
            if trade.time.as_u64() < oldest || cutoff.is_some_and(|boundary| trade.time > boundary)
            {
                continue;
            }
            let day = day_start(trade.time);
            let minute = trade.time.as_u64() / ONE_MIN_MS * ONE_MIN_MS;
            let notional = trade_notional(*trade, qty_is_quote);
            let signed = if trade.is_sell { -notional } else { notional };
            *stage
                .days
                .entry(day)
                .or_default()
                .entry(minute)
                .or_default() += signed;
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

        let exact = |value: f64| format!("${}", format_with_commas(value));
        let signed_exact = move |value: f64| {
            format!(
                "{}{}",
                if value >= 0.0 { "+" } else { "-" },
                exact(value.abs())
            )
        };
        let tooltip = move |candle: &DeltaCandle, _next: Option<&DeltaCandle>| {
            PlotTooltip::new(format!(
                "CVD: {}\nO {}  H {}\nL {}  C {}\nΔ {}",
                exact(candle.close),
                exact(candle.open),
                exact(candle.high),
                exact(candle.low),
                exact(candle.close),
                signed_exact(candle.delta),
            ))
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
        if let Some(rebuild_from) = self.merged.refresh(&self.inner, self.lookback_days) {
            self.assemble_candles_from(rebuild_from);
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
        let mut cumulative = self
            .candles
            .range(..rebuild_key)
            .next_back()
            .map_or(0.0, |(_, candle)| candle.close);
        drop(self.candles.split_off(&rebuild_key));

        for day in &self.merged.days {
            if day.day_ts.saturating_add(DAY_MS) <= rebuild_from {
                continue;
            }
            for (bucket_start, delta) in &day.deltas {
                if *bucket_start < rebuild_from {
                    continue;
                }
                let bucket = bucket_start / interval_ms * interval_ms;
                let after = cumulative + delta;
                let candle = self
                    .candles
                    .entry(UnixMs::new(bucket))
                    .or_insert(DeltaCandle {
                        open: cumulative,
                        high: cumulative.max(after),
                        low: cumulative.min(after),
                        close: after,
                        delta: 0.0,
                    });
                candle.close = after;
                candle.high = candle.high.max(after);
                candle.low = candle.low.min(after);
                candle.delta += delta;
                cumulative = after;
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
        self.minute_history
            .persist(source, day_start.as_u64(), covered_through);
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
        let mut dirty_from = None::<u64>;
        for (source, day, deltas) in committed {
            dirty_from = Some(dirty_from.map_or(day, |current| current.min(day)));
            self.inner.merge_exact_cvd_minutes(source, day, &deltas);
        }
        if let Some(dirty_from) = dirty_from {
            self.merged.mark_dirty_from(dirty_from);
            self.refresh_merged();
        }
    }

    fn discard_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.minute_history.discard(req_id);
    }
}

struct MergedDay {
    day_ts: u64,
    deltas: BTreeMap<u64, f64>,
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
                        deltas: merged_day_deltas_from(inner, day_ts, day_ts),
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
                let day_from = if day.day_ts == from_day {
                    from
                } else {
                    day.day_ts
                };
                drop(day.deltas.split_off(&day_from));
                day.deltas
                    .extend(merged_day_deltas_from(inner, day.day_ts, day_from));
            }
            Some(from)
        } else {
            None
        }
    }
}

fn merged_day_deltas_from(
    inner: &FootprintHistoryIndicator,
    day_ts: u64,
    from: u64,
) -> BTreeMap<u64, f64> {
    inner.merged_cvd_deltas_from(day_ts, from)
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
        std::fs::remove_file(&path).expect("remove cache test file");
        std::fs::remove_dir(&root).expect("remove cache test directory");
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
