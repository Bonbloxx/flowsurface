use super::KlineIndicatorImpl;
use super::footprint_history::{
    DAY_MS, DayStats, DisplayDay, FootprintHistoryIndicator, LevelStats, day_start, draw_text,
    group_levels, trade_notional, utc_days_covering,
};
use crate::chart::{Message, ViewState};

use std::cell::RefCell;
use std::collections::BTreeMap;

use data::aggr::ticks::TickAggr;
use data::aggr::time::TimeSeries;
use data::chart::PlotData;
use data::chart::kline::{KlineDataPoint, KlineTrades};
use exchange::unit::{Price, PriceStep, qty::volume_size_unit};
use exchange::{SizeUnit, Trade, UnixMs, adapter::MarketKind};
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Alignment, Color, Element, Point, Rectangle, Size};
use rustc_hash::FxHashSet;

/// Daily dollar-delta profile overlay. Reuses Footprint History's UTC-day
/// trade book and venue aggregation; only the chart drawing is new.
///
/// Aggregated per-day profiles are cached so panning and zooming never
/// re-absorb whole days of klines per frame. Invalidation is precise:
/// config-level changes rebuild every day, while incoming trades/klines only
/// rebuild the UTC days they touch (live ticks therefore just refresh today).
pub struct DailyDeltaIndicator {
    inner: FootprintHistoryIndicator,
    lookback_days: usize,
    /// Bumped when inputs affecting *every* displayed day change
    /// (sources, aggregation config, basis, tick size, full rebuilds).
    full_rev: u64,
    cache: RefCell<Option<OverlayCache>>,
    loaded_cache_days: FxHashSet<(exchange::Ticker, u64)>,
}

struct OverlayCache {
    full_rev: u64,
    group_step_units: i64,
    lookback_days: usize,
    today: u64,
    qty_is_quote: bool,
    /// Days at/after this UTC-day start are stale and must be rebuilt
    /// before the next draw.
    dirty_from_day: Option<u64>,
    days: Vec<CachedDayProfile>,
}

struct CachedDayProfile {
    day_ts: u64,
    grouped: BTreeMap<i64, LevelStats>,
    max_abs: f64,
    max_abs_price: Option<i64>,
    poc: Option<i64>,
    trade_high: Option<i64>,
    trade_low: Option<i64>,
    kline_high: Option<i64>,
    kline_low: Option<i64>,
    profile_high: Option<i64>,
    profile_low: Option<i64>,
    from_footprint_fallback: bool,
}

impl CachedDayProfile {
    fn refresh_price_span(&mut self) {
        self.profile_high = self.trade_high.into_iter().chain(self.kline_high).max();
        self.profile_low = self.trade_low.into_iter().chain(self.kline_low).min();
    }
}

/// Directional colors shared by Delta History and other profile-style charts.
pub(crate) fn delta_history_colors(palette: &Extended) -> (Color, Color) {
    if palette.is_dark {
        (
            Color::from_rgb(0.27, 0.48, 0.68),
            Color::from_rgb(0.55, 0.28, 0.28),
        )
    } else {
        (palette.success.base.color, palette.danger.base.color)
    }
}

impl DailyDeltaIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(4);
        Self {
            inner,
            lookback_days: 4,
            full_rev: 0,
            cache: RefCell::new(None),
            loaded_cache_days: FxHashSet::default(),
        }
    }

    fn mark_all_changed(&mut self) {
        self.full_rev = self.full_rev.wrapping_add(1);
    }

    /// Mark the UTC day containing `earliest_time` (and everything after it)
    /// as stale. No-op while no cache exists — the next draw builds fresh.
    fn mark_days_dirty(&mut self, earliest_time: u64) {
        let from = day_start(UnixMs::new(earliest_time));
        if let Some(cache) = self.cache.borrow_mut().as_mut() {
            cache.dirty_from_day = Some(
                cache
                    .dirty_from_day
                    .map_or(from, |current| current.min(from)),
            );
        }
    }

    fn build_day_profile(
        &self,
        data_source: &PlotData<KlineDataPoint>,
        day_ts: u64,
        group_step: PriceStep,
        qty_is_quote: bool,
    ) -> CachedDayProfile {
        let mut day = self.inner.display_day_at(day_ts);
        let from_footprint_fallback = day.stats.levels.is_empty();
        let trade_high = day.stats.high.map(|price| Price::from_f64(price).units);
        let trade_low = day.stats.low.map(|price| Price::from_f64(price).units);
        let (kline_high, kline_low) = kline_day_price_range(data_source, day_ts);
        apply_kline_day(&mut day, data_source, day_ts, group_step, qty_is_quote);
        let mut profile = CachedDayProfile {
            day_ts,
            trade_high,
            trade_low,
            kline_high,
            kline_low,
            from_footprint_fallback,
            ..build_cached_profile(&day.stats, group_step)
        };
        profile.refresh_price_span();
        profile
    }

    /// Apply live executions directly to the already-grouped current-day
    /// profile. The authoritative venue day books are still updated first;
    /// this only avoids cloning and regrouping the whole multi-venue day on
    /// every 33 ms WebSocket batch.
    fn apply_live_trades_to_cache(&mut self, trades: &[Trade]) -> bool {
        let mut cache = self.cache.borrow_mut();
        let Some(cache) = cache.as_mut() else {
            return false;
        };
        let step_units = cache.group_step_units;
        if step_units <= 0 {
            return false;
        }
        let today = cache.today;
        let qty_is_quote = cache.qty_is_quote;
        let Some(profile) = cache
            .days
            .iter_mut()
            .find(|profile| profile.day_ts == today)
        else {
            return false;
        };
        // The first cache can be built from candle footprints before the
        // shared history book hydrates. Rebuild once when that fallback hands
        // over to the authoritative day book instead of double-counting it.
        if profile.from_footprint_fallback {
            return false;
        }

        let mut changed = false;
        for trade in trades {
            if day_start(trade.time) != today {
                continue;
            }
            let bucket = trade.price.units.div_euclid(step_units) * step_units;
            let notional = trade_notional(*trade, qty_is_quote);
            let (old_abs, new_abs, new_volume) = {
                let level = profile.grouped.entry(bucket).or_default();
                let old_abs = level.delta().abs();
                level.add_notional(trade.is_sell, notional);
                (old_abs, level.delta().abs(), level.volume())
            };

            if new_abs >= profile.max_abs {
                profile.max_abs = new_abs;
                profile.max_abs_price = Some(bucket);
            } else if profile.max_abs_price == Some(bucket) && new_abs < old_abs {
                let replacement = profile
                    .grouped
                    .iter()
                    .max_by(|left, right| left.1.delta().abs().total_cmp(&right.1.delta().abs()))
                    .map(|(price, level)| (*price, level.delta().abs()));
                profile.max_abs_price = replacement.map(|(price, _)| price);
                profile.max_abs = replacement.map_or(0.0, |(_, delta)| delta);
            }

            let poc_volume = profile
                .poc
                .and_then(|price| profile.grouped.get(&price))
                .map_or(0.0, |level| level.volume());
            if profile.poc.is_none() || new_volume > poc_volume {
                profile.poc = Some(bucket);
            }

            profile.trade_high = Some(
                profile
                    .trade_high
                    .map_or(trade.price.units, |high| high.max(trade.price.units)),
            );
            profile.trade_low = Some(
                profile
                    .trade_low
                    .map_or(trade.price.units, |low| low.min(trade.price.units)),
            );
            profile.refresh_price_span();
            changed = true;
        }
        changed
    }

    fn apply_klines_to_cache(
        &mut self,
        klines: &[exchange::Kline],
        source: &PlotData<KlineDataPoint>,
    ) -> bool {
        let mut cache = self.cache.borrow_mut();
        let Some(cache) = cache.as_mut() else {
            return false;
        };
        let touched_days = klines
            .iter()
            .map(|kline| day_start(kline.time))
            .collect::<FxHashSet<_>>();
        for day in touched_days {
            let Some(profile) = cache.days.iter_mut().find(|profile| profile.day_ts == day) else {
                continue;
            };
            // With no authoritative levels yet, candle footprints are also
            // the volume source and must be reabsorbed by the full builder.
            if profile.from_footprint_fallback && matches!(source, PlotData::TickBased(_)) {
                return false;
            }
            (profile.kline_high, profile.kline_low) = kline_day_price_range(source, day);
            profile.refresh_price_span();
        }
        true
    }
}

impl KlineIndicatorImpl for DailyDeltaIndicator {
    fn clear_all_caches(&mut self) {
        self.inner.clear_all_caches();
    }

    fn clear_crosshair_caches(&mut self) {
        self.inner.clear_crosshair_caches();
    }

    fn element<'a>(
        &'a self,
        _chart: &'a ViewState,
        _data_labels_always_visible: bool,
        _visible_range: std::ops::RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        iced::widget::row![].into()
    }

    fn configure_footprint_history(&mut self, sources: &[exchange::TickerInfo], aggregate: bool) {
        self.inner.configure_footprint_history(sources, aggregate);
        self.inner.set_lookback_days(self.lookback_days as u16);
        self.mark_all_changed();
    }

    fn set_trade_history_lookback(&mut self, days: u16) {
        self.lookback_days = usize::from(days).clamp(1, 30);
        self.inner.set_lookback_days(days);
    }

    fn reset_trade_history_backfill(&mut self) {
        self.inner.reset_trade_history_backfill();
        self.loaded_cache_days.clear();
        self.mark_all_changed();
    }

    fn prepare_footprint_history(&mut self, source: exchange::TickerInfo, cutoff: UnixMs) {
        self.inner.prepare_footprint_history(source, cutoff);
    }

    fn load_cached_footprint_day(
        &mut self,
        source: exchange::TickerInfo,
        day_start: UnixMs,
    ) -> Option<UnixMs> {
        let loaded = self.inner.load_cached_footprint_day(source, day_start);
        if loaded.is_some()
            && self
                .loaded_cache_days
                .insert((source.ticker, day_start.as_u64()))
        {
            self.mark_days_dirty(day_start.as_u64());
        }
        loaded
    }

    fn persist_cached_footprint_day(
        &mut self,
        source: exchange::TickerInfo,
        day_start: UnixMs,
        covered_through: UnixMs,
    ) {
        self.inner
            .persist_cached_footprint_day(source, day_start, covered_through);
    }

    fn on_source_trades(
        &mut self,
        source: exchange::TickerInfo,
        trades: &[exchange::Trade],
        historical: bool,
    ) {
        self.inner.on_source_trades(source, trades, historical);
        if let Some(earliest) = trades.iter().map(|trade| trade.time.as_u64()).min() {
            let applied_incrementally = !historical && self.apply_live_trades_to_cache(trades);
            if !applied_incrementally {
                self.mark_days_dirty(earliest);
            }
        }
    }

    fn stage_source_trades(
        &mut self,
        req_id: uuid::Uuid,
        source: exchange::TickerInfo,
        trades: &[exchange::Trade],
    ) {
        self.inner.stage_source_trades(req_id, source, trades);
    }

    fn commit_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.inner.commit_staged_source_trades(req_id);
        self.mark_all_changed();
    }

    fn discard_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.inner.discard_staged_source_trades(req_id);
    }

    fn on_insert_klines(&mut self, klines: &[exchange::Kline], source: &PlotData<KlineDataPoint>) {
        if let Some(earliest) = klines.iter().map(|kline| kline.time.as_u64()).min()
            && !self.apply_klines_to_cache(klines, source)
        {
            self.mark_days_dirty(earliest);
        }
    }

    fn on_insert_trades(
        &mut self,
        _trades: &[Trade],
        _old_dp_len: usize,
        _source: &PlotData<KlineDataPoint>,
    ) {
        // Live executions already enter through `on_source_trades` above.
        // Marking the day dirty again here defeated the incremental update and
        // forced a complete multi-venue regroup on every market batch.
    }

    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.mark_all_changed();
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.mark_all_changed();
    }

    fn on_basis_change(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.mark_all_changed();
    }

    fn on_source_open_interest(
        &mut self,
        source: exchange::TickerInfo,
        values: &[exchange::OpenInterest],
    ) {
        self.inner.on_source_open_interest(source, values);
    }

    fn draw_overlay(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        group_step: PriceStep,
    ) {
        if group_step.units <= 0 {
            return;
        }

        let now = UnixMs::now();
        let today = day_start(now);
        let (highest, lowest) = chart.price_range(&region);
        let scaling = chart.scaling.max(0.01);
        let current_w = (88.0 / scaling).clamp(36.0, 96.0);
        let history_w = (64.0 / scaling).clamp(28.0, 72.0);
        let pad = 8.0 / scaling;
        let (buy, sell) = delta_history_colors(palette);
        let axis = palette.background.strong.color.scale_alpha(0.75);
        let label = palette.background.base.text;
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || chart.ticker_info.market_type() == MarketKind::InversePerps;

        // Rebuild aggregated profiles only when inputs actually changed.
        // Panning and zooming reuse the cache instead of re-absorbing every
        // kline of every displayed day on each frame; live ticks only refresh
        // the days they touched.
        {
            let needs_full_rebuild = match self.cache.borrow().as_ref() {
                None => true,
                Some(cache) => {
                    cache.full_rev != self.full_rev
                        || cache.group_step_units != group_step.units
                        || cache.lookback_days != self.lookback_days
                        || cache.today != today
                }
            };
            if needs_full_rebuild {
                let days = display_day_starts(now, self.lookback_days)
                    .into_iter()
                    .map(|day_ts| {
                        self.build_day_profile(data_source, day_ts, group_step, qty_is_quote)
                    })
                    .collect::<Vec<_>>();
                *self.cache.borrow_mut() = Some(OverlayCache {
                    full_rev: self.full_rev,
                    group_step_units: group_step.units,
                    lookback_days: self.lookback_days,
                    today,
                    qty_is_quote,
                    dirty_from_day: None,
                    days,
                });
            } else {
                let mut cache = self.cache.borrow_mut();
                if let Some(cache) = cache.as_mut() {
                    let dirty_from = cache.dirty_from_day.take();
                    for profile in &mut cache.days {
                        if dirty_from.is_some_and(|from| profile.day_ts >= from) {
                            *profile = self.build_day_profile(
                                data_source,
                                profile.day_ts,
                                group_step,
                                qty_is_quote,
                            );
                        }
                    }
                }
            }
        }
        let cache = self.cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return;
        };

        for profile in &cache.days {
            if profile.profile_high.is_none()
                && profile.profile_low.is_none()
                && profile.grouped.is_empty()
            {
                continue;
            }
            let is_current = profile.day_ts == today;
            let profile_w = if is_current { current_w } else { history_w };
            let Some(x) = day_x(
                chart,
                data_source,
                region,
                pad,
                profile.day_ts,
                is_current,
                now,
            ) else {
                continue;
            };
            let poc_line =
                poc_line_range(chart, data_source, region, x, profile.day_ts, is_current);
            let profile_visible = profile_intersects_region(region, x, profile_w);
            if !profile_visible && poc_line.is_none() {
                continue;
            }
            if !profile_visible {
                if let Some(line) = poc_line {
                    draw_profile_poc(frame, chart, profile, group_step, line, buy, scaling);
                }
                continue;
            }

            draw_day_profile(
                frame, chart, profile, group_step, x, profile_w, highest, lowest, scaling, buy,
                sell, axis, label, is_current, poc_line,
            );
        }
    }
}

fn day_x(
    chart: &ViewState,
    data_source: &PlotData<KlineDataPoint>,
    region: Rectangle,
    pad: f32,
    day_ts: u64,
    is_current: bool,
    now: UnixMs,
) -> Option<f32> {
    let x = match data_source {
        PlotData::TimeBased(_) => {
            if is_current {
                let (visible_earliest, visible_latest) = chart.interval_range(&region);
                // Pin to the live edge only while today is actually in view.
                // Otherwise the developing profile overlays historical candles.
                if now.as_u64() < visible_earliest.saturating_sub(DAY_MS)
                    || now.as_u64() > visible_latest.saturating_add(DAY_MS)
                {
                    return None;
                }
                region.x + region.width - pad
            } else {
                // Completed UTC days are keyed at 00:00 (the open). That is the
                // previous day's close on the time axis, so park the spine at
                // this day's close instead.
                chart.interval_to_x(completed_day_close_ts(day_ts))
            }
        }
        PlotData::TickBased(tick_aggr) => tick_day_x(chart, tick_aggr, day_ts)?,
    };

    Some(x)
}

fn profile_intersects_region(region: Rectangle, profile_x: f32, profile_w: f32) -> bool {
    profile_x >= region.x && profile_x - profile_w <= region.x + region.width
}

fn display_day_starts(now: UnixMs, cap: usize) -> Vec<u64> {
    let cap = cap.max(1);
    let today = day_start(now);
    utc_days_covering(
        UnixMs::new(today.saturating_sub((cap.saturating_sub(1) as u64) * DAY_MS)),
        now,
        cap,
    )
    .into_iter()
    .map(|(start, _)| start.as_u64())
    .collect()
}

fn completed_day_close_ts(day_ts: u64) -> u64 {
    day_ts.saturating_add(DAY_MS)
}

fn tick_day_x(chart: &ViewState, tick_aggr: &TickAggr, day_ts: u64) -> Option<f32> {
    let day_end = completed_day_close_ts(day_ts);
    let len = tick_aggr.datapoints.len();
    let index = tick_aggr
        .datapoints
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, dp)| {
            let time = dp.kline.time.as_u64();
            (time >= day_ts && time < day_end).then_some((len - 1 - i) as u64)
        })?;
    Some(chart.interval_to_x(index))
}

fn poc_line_range(
    chart: &ViewState,
    data_source: &PlotData<KlineDataPoint>,
    region: Rectangle,
    profile_x: f32,
    day_ts: u64,
    is_current: bool,
) -> Option<(f32, f32)> {
    let next_day = completed_day_close_ts(day_ts);
    let end = match data_source {
        PlotData::TimeBased(_) => chart.interval_to_x(if is_current {
            next_day
        } else {
            completed_day_close_ts(next_day)
        }),
        PlotData::TickBased(tick_aggr) => tick_day_x(chart, tick_aggr, next_day)?,
    };
    let start = profile_x.max(region.x);
    let end = end.min(region.x + region.width);
    (end > start).then_some((start, end))
}

fn kline_day_price_range(
    data_source: &PlotData<KlineDataPoint>,
    day_ts: u64,
) -> (Option<i64>, Option<i64>) {
    let day_end = day_ts.saturating_add(DAY_MS);
    let mut high = None::<Price>;
    let mut low = None::<Price>;
    let mut visit = |kline: &exchange::Kline| {
        high = Some(high.map_or(kline.high, |current| current.max(kline.high)));
        low = Some(low.map_or(kline.low, |current| current.min(kline.low)));
    };
    match data_source {
        PlotData::TimeBased(series) => {
            for (_, datapoint) in series
                .datapoints
                .range(UnixMs::new(day_ts)..UnixMs::new(day_end))
            {
                visit(&datapoint.kline);
            }
        }
        PlotData::TickBased(tick_aggr) => {
            for datapoint in &tick_aggr.datapoints {
                let time = datapoint.kline.time.as_u64();
                if time >= day_ts && time < day_end {
                    visit(&datapoint.kline);
                }
            }
        }
    }
    (high.map(|price| price.units), low.map(|price| price.units))
}

fn apply_kline_day(
    day: &mut DisplayDay,
    data_source: &PlotData<KlineDataPoint>,
    day_ts: u64,
    step: PriceStep,
    qty_is_quote: bool,
) {
    let day_end = day_ts.saturating_add(DAY_MS);
    // The shared history book already receives the same live and historical
    // prints as the main chart. Re-absorbing a populated candle footprint
    // doubles every level. The chart footprint is only a fallback for the
    // initial raw-trade buffer that predates the indicator's live book.
    let include_footprint = day.stats.levels.is_empty();
    match data_source {
        PlotData::TimeBased(series) => {
            absorb_time_series(
                &mut day.stats,
                series,
                day_ts,
                day_end,
                step,
                include_footprint,
                qty_is_quote,
            );
        }
        PlotData::TickBased(tick_aggr) => {
            for dp in &tick_aggr.datapoints {
                let time = dp.kline.time.as_u64();
                if time >= day_ts && time < day_end {
                    absorb_ohlc(&mut day.stats, dp.kline.high, dp.kline.low);
                    if include_footprint {
                        absorb_footprint(&mut day.stats, &dp.footprint, step, qty_is_quote);
                    }
                }
            }
        }
    }
}

fn absorb_time_series(
    stats: &mut DayStats,
    series: &TimeSeries<KlineDataPoint>,
    day_ts: u64,
    day_end: u64,
    step: PriceStep,
    include_footprint: bool,
    qty_is_quote: bool,
) {
    let start = UnixMs::new(day_ts);
    let end = UnixMs::new(day_end);
    for (_, dp) in series.datapoints.range(start..end) {
        absorb_ohlc(stats, dp.kline.high, dp.kline.low);
        if include_footprint {
            absorb_footprint(stats, &dp.footprint, step, qty_is_quote);
        }
    }
}

fn absorb_ohlc(stats: &mut DayStats, high: Price, low: Price) {
    let high = high.to_f64();
    let low = low.to_f64();
    stats.high = Some(stats.high.map_or(high, |value| value.max(high)));
    stats.low = Some(stats.low.map_or(low, |value| value.min(low)));
}

fn absorb_footprint(
    stats: &mut DayStats,
    footprint: &KlineTrades,
    step: PriceStep,
    qty_is_quote: bool,
) {
    if step.units <= 0 {
        return;
    }
    for (price, group) in &footprint.trades {
        let bucket = price.units.div_euclid(step.units) * step.units;
        let notional = |qty: f64| {
            if qty_is_quote {
                qty
            } else {
                price.to_f64() * qty
            }
        };
        let level = stats.levels.entry(bucket).or_default();
        level.add_notional(false, notional(group.buy_qty.to_f64()));
        level.add_notional(true, notional(group.sell_qty.to_f64()));
    }
}

/// Aggregate a merged day into the view-independent numbers the overlay draws:
/// grouped levels, delta scale, POC and the profile's price span.
fn build_cached_profile(stats: &DayStats, step: PriceStep) -> CachedDayProfile {
    let grouped = group_levels(stats, step);
    let max_abs_entry = grouped
        .iter()
        .max_by(|left, right| left.1.delta().abs().total_cmp(&right.1.delta().abs()));
    let max_abs = max_abs_entry.map_or(0.0, |(_, level)| level.delta().abs());
    let max_abs_price = max_abs_entry.map(|(price, _)| *price);
    let poc = grouped
        .iter()
        .max_by(|left, right| left.1.volume().total_cmp(&right.1.volume()))
        .map(|(price_units, _)| *price_units);
    let profile_high = stats
        .high
        .map(|price| Price::from_f64(price).units)
        .or_else(|| grouped.keys().next_back().copied());
    let profile_low = stats
        .low
        .map(|price| Price::from_f64(price).units)
        .or_else(|| grouped.keys().next().copied());

    CachedDayProfile {
        day_ts: 0,
        grouped,
        max_abs,
        max_abs_price,
        poc,
        trade_high: profile_high,
        trade_low: profile_low,
        kline_high: None,
        kline_low: None,
        profile_high,
        profile_low,
        from_footprint_fallback: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_day_profile(
    frame: &mut canvas::Frame,
    chart: &ViewState,
    profile: &CachedDayProfile,
    group_step: PriceStep,
    x: f32,
    profile_w: f32,
    highest: Price,
    lowest: Price,
    scaling: f32,
    buy: Color,
    sell: Color,
    axis: Color,
    label: Color,
    is_current: bool,
    poc_line: Option<(f32, f32)>,
) {
    let CachedDayProfile {
        grouped,
        max_abs,
        profile_high,
        profile_low,
        ..
    } = profile;
    let step_u = group_step.units.max(1);

    let (Some(high_u), Some(low_u)) = (*profile_high, *profile_low) else {
        return;
    };
    if high_u < low_u {
        return;
    }
    let low_aligned = low_u.div_euclid(step_u) * step_u;
    let high_aligned = high_u.div_euclid(step_u) * step_u;
    let top_y = chart.price_to_y(Price::from_units(high_aligned.saturating_add(step_u)));
    let bot_y = chart.price_to_y(Price::from_units(low_aligned));

    frame.fill_rectangle(
        Point::new(x, top_y),
        Size::new(1.0 / scaling, (bot_y - top_y).abs()),
        axis,
    );

    let min_bar_h = 1.0 / scaling;
    let show_text_h = 8.0 / scaling;
    let text_size = (9.0 / scaling).clamp(7.0, 11.0);
    let divider = Color::from_rgba(0.0, 0.0, 0.0, 0.45);
    let divider_h = (0.8 / scaling).clamp(0.4, 1.1);

    // When configured price rows are smaller than one screen pixel, drawing
    // every row creates thousands of fully-overlapping rectangles during a
    // pan/zoom. Merge only for presentation at the current pixel resolution;
    // the cached source levels remain exact and are revealed again on zoom-in.
    let configured_row_px =
        (step_u as f32 / chart.effective_tick_units() as f32) * chart.cell_height * scaling;
    let rows_per_pixel = if configured_row_px.is_finite() && configured_row_px > 0.0 {
        (1.0 / configured_row_px).ceil().max(1.0) as i64
    } else {
        1
    };
    let visible_low = lowest.units.min(highest.units);
    let visible_high = lowest.units.max(highest.units);
    let (render_step_u, render_rows) =
        delta_rows_for_view(grouped, step_u, rows_per_pixel, visible_low, visible_high);
    let render_max_abs = if rows_per_pixel == 1 {
        *max_abs
    } else {
        render_rows
            .iter()
            .map(|(_, delta)| delta.abs())
            .fold(0.0_f64, f64::max)
    };

    for (price_units, delta) in render_rows {
        if delta == 0.0 || render_max_abs <= 0.0 {
            continue;
        }
        let price = Price::from_units(price_units);
        let next = Price::from_units(price_units.saturating_add(render_step_u));
        if next < lowest || price > highest {
            continue;
        }
        let y_top = chart.price_to_y(next);
        let y_bot = chart.price_to_y(price);
        let bar_h = (y_bot - y_top).abs().max(min_bar_h);
        if !bar_h.is_finite() {
            continue;
        }
        let y = y_top.min(y_bot);
        let width = ((delta.abs() / render_max_abs) as f32 * profile_w).max(4.0 / scaling);
        let color = if delta >= 0.0 { buy } else { sell };
        frame.fill_rectangle(Point::new(x - width, y), Size::new(width, bar_h), color);
        let line_h = divider_h.min(bar_h * 0.2);
        if line_h > 0.0 {
            frame.fill_rectangle(Point::new(x - width, y), Size::new(width, line_h), divider);
        }
        // Labels are anchored to the profile spine and extend left; they do not
        // need to fit inside the bar itself. Suppressing narrow bars hid valid
        // delta values even when the price row was tall enough to read them.
        if delta_label_fits_row(bar_h, show_text_h) {
            draw_text(
                frame,
                &profile_delta_label(delta),
                Point::new(x - 3.0 / scaling, y + bar_h * 0.5),
                text_size,
                label,
                Alignment::End,
            );
        }
    }

    if let Some(line) = poc_line {
        draw_profile_poc(frame, chart, profile, group_step, line, buy, scaling);
    }

    if is_current {
        draw_text(
            frame,
            "today",
            Point::new(x - 4.0 / scaling, top_y - 10.0 / scaling),
            (10.0 / scaling).clamp(7.0, 12.0),
            label.scale_alpha(0.7),
            Alignment::End,
        );
    }
}

fn draw_profile_poc(
    frame: &mut canvas::Frame,
    chart: &ViewState,
    profile: &CachedDayProfile,
    group_step: PriceStep,
    (line_start, line_end): (f32, f32),
    color: Color,
    scaling: f32,
) {
    let Some(poc_units) = profile.poc else {
        return;
    };
    let step_units = group_step.units.max(1);
    let price = Price::from_units(poc_units);
    let next = Price::from_units(poc_units.saturating_add(step_units));
    let poc_y = (chart.price_to_y(price) + chart.price_to_y(next)) * 0.5;
    frame.stroke(
        &Path::line(Point::new(line_start, poc_y), Point::new(line_end, poc_y)),
        Stroke::with_color(
            Stroke {
                width: 2.0 / scaling,
                ..Stroke::default()
            },
            color,
        ),
    );
}

fn delta_rows_for_view(
    grouped: &BTreeMap<i64, LevelStats>,
    step_units: i64,
    rows_per_pixel: i64,
    visible_low: i64,
    visible_high: i64,
) -> (i64, Vec<(i64, f64)>) {
    let step_units = step_units.max(1);
    let render_step = step_units
        .saturating_mul(rows_per_pixel.max(1))
        .max(step_units);
    let range_low = visible_low.div_euclid(render_step) * render_step;
    let range_high = visible_high.saturating_add(render_step);
    let mut rows = Vec::<(i64, f64)>::new();
    for (price_units, level) in grouped.range(range_low..=range_high) {
        let bucket = price_units.div_euclid(render_step) * render_step;
        let delta = level.delta();
        if let Some((last_bucket, last_delta)) = rows.last_mut()
            && *last_bucket == bucket
        {
            *last_delta += delta;
        } else {
            rows.push((bucket, delta));
        }
    }
    (render_step, rows)
}

fn delta_label_fits_row(bar_height: f32, minimum_text_height: f32) -> bool {
    bar_height >= minimum_text_height
}

fn profile_delta_label(value: f64) -> String {
    let abs = value.abs();
    let sign = if value < 0.0 { "-" } else { "" };
    if abs >= 1_000_000.0 {
        format!("{sign}{}m", trim_decimals(abs / 1_000_000.0))
    } else if abs >= 1_000.0 {
        format!("{sign}{}k", trim_decimals(abs / 1_000.0))
    } else {
        format!("{sign}{abs:.0}")
    }
}

fn trim_decimals(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use data::chart::{Basis, ViewConfig};
    use exchange::{Kline, TickerInfo, Timeframe, Trade, Volume, adapter::Exchange, unit::Qty};

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    fn source() -> TickerInfo {
        TickerInfo::new(
            exchange::Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        )
    }

    #[test]
    fn grouped_levels_span_traded_prices() {
        let mut indicator = DailyDeltaIndicator::new();
        indicator.set_trade_history_lookback(4);
        let source = source();
        indicator.configure_footprint_history(&[source], true);
        let today = day_start(UnixMs::now());
        let trades: Vec<Trade> = (0..40)
            .map(|i| {
                trade(
                    today + 60_000 * i as u64,
                    62_000.0 + f64::from(i) * 10.0,
                    1.0,
                    i % 2 == 0,
                )
            })
            .collect();
        indicator.on_source_trades(source, &trades, true);

        let days = indicator.inner.display_day_list(UnixMs::now(), 5);
        assert!(!days[0].stats.levels.is_empty());
        let step = PriceStep {
            units: Price::from_f64(5.0).units,
        };
        let grouped = group_levels(&days[0].stats, step);
        assert!(
            grouped.len() > 10,
            "expected many $5 buckets, got {}",
            grouped.len()
        );
    }

    #[test]
    fn profile_labels_use_millions() {
        assert_eq!(profile_delta_label(1_500_000.0), "1.5m");
        assert_eq!(profile_delta_label(-2_000_000.0), "-2m");
        assert_eq!(profile_delta_label(383_500.0), "383.5k");
    }

    #[test]
    fn delta_label_visibility_only_requires_readable_row_height() {
        assert!(delta_label_fits_row(12.0, 8.0));
        assert!(!delta_label_fits_row(7.0, 8.0));
    }

    #[test]
    fn display_day_starts_respects_lookback_cap() {
        let now = UnixMs::new(20 * DAY_MS + 3_600_000);
        let days = display_day_starts(now, 5);
        assert_eq!(days.len(), 5);
        assert_eq!(days[0], 16 * DAY_MS);
        assert_eq!(*days.last().unwrap(), 20 * DAY_MS);
    }

    #[test]
    fn completed_day_sits_at_that_days_close_not_its_open() {
        let tuesday_open = 10 * DAY_MS;
        assert_eq!(completed_day_close_ts(tuesday_open), 11 * DAY_MS);
    }

    #[test]
    fn one_minute_poc_remains_visible_after_profile_spine_leaves_view() {
        let step = PriceStep::from(source().min_ticksize);
        let mut chart = ViewState::new(
            Basis::Time(Timeframe::M1),
            step,
            step.decimal_places(),
            source(),
            ViewConfig::default(),
            10.0,
            10.0,
        );
        chart.latest_x = 3 * DAY_MS;
        let data_source =
            PlotData::TimeBased(TimeSeries::<KlineDataPoint>::new(Timeframe::M1, step, &[]));
        let region = Rectangle {
            x: chart.interval_to_x(DAY_MS + DAY_MS / 2),
            y: 0.0,
            width: 120.0 * chart.cell_width,
            height: 100.0,
        };

        let profile_x = day_x(
            &chart,
            &data_source,
            region,
            8.0,
            0,
            false,
            UnixMs::new(3 * DAY_MS),
        )
        .expect("completed-day profile must retain its time-axis anchor");
        assert!(profile_x < region.x);
        assert!(!profile_intersects_region(region, profile_x, 64.0));

        let poc_line = poc_line_range(&chart, &data_source, region, profile_x, 0, false)
            .expect("the forward POC interval crosses the one-minute viewport");
        assert_eq!(poc_line, (region.x, region.x + region.width));
    }

    #[test]
    fn kline_wick_extends_day_low_below_traded_prices() {
        let mut stats = DayStats::default();
        stats.high = Some(62_965.0);
        stats.low = Some(62_965.0);
        absorb_ohlc(
            &mut stats,
            Price::from_f64(63_595.1),
            Price::from_f64(62_484.2),
        );
        assert_eq!(stats.low, Some(62_484.2));
        assert_eq!(stats.high, Some(63_595.1));
    }

    #[test]
    fn populated_history_book_does_not_double_count_the_chart_footprint() {
        let day = day_start(UnixMs::now());
        let price = Price::from_f64(67_000.0);
        let trade = trade(day + 60_000, 67_000.0, 1.0, false);
        let step = PriceStep::from(source().min_ticksize);
        let kline = Kline {
            time: UnixMs::new(day),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::empty_buy_sell(),
        };
        let series =
            TimeSeries::<KlineDataPoint>::new(Timeframe::M5, step, &[kline]).with_trades(&[trade]);
        let mut display = DisplayDay::default();
        display
            .stats
            .levels
            .entry(price.units)
            .or_default()
            .add_notional(false, 67_000.0);
        let before = display.stats.levels[&price.units].volume();

        apply_kline_day(&mut display, &PlotData::TimeBased(series), day, step, false);

        assert_eq!(display.stats.levels[&price.units].volume(), before);
        assert_eq!(display.stats.levels.len(), 1);
    }

    #[test]
    fn footprint_fallback_normalizes_base_and_quote_quantity_once() {
        let step = PriceStep::from(source().min_ticksize);
        let price = Price::from_f64(100.0);
        let mut base = KlineTrades::new();
        base.add_trade_to_nearest_bin(&trade(1, 100.0, 2.0, false), step);
        let mut quote = KlineTrades::new();
        quote.add_trade_to_nearest_bin(&trade(1, 100.0, 200.0, false), step);
        let mut base_stats = DayStats::default();
        let mut quote_stats = DayStats::default();

        absorb_footprint(&mut base_stats, &base, step, false);
        absorb_footprint(&mut quote_stats, &quote, step, true);

        assert_eq!(base_stats.levels[&price.units].volume(), 200.0);
        assert_eq!(quote_stats.levels[&price.units].volume(), 200.0);
    }

    #[test]
    fn live_incremental_profile_matches_authoritative_full_rebuild() {
        let source = source();
        let today = day_start(UnixMs::now());
        let group_step = PriceStep {
            units: Price::from_f64(1.0).units,
        };
        let data_source = PlotData::TimeBased(TimeSeries::<KlineDataPoint>::new(
            Timeframe::M1,
            PriceStep::from(source.min_ticksize),
            &[],
        ));
        let mut indicator = DailyDeltaIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator.on_source_trades(
            source,
            &[
                trade(today + 1_000, 100.0, 10.0, false),
                trade(today + 2_000, 101.0, 5.0, false),
            ],
            true,
        );
        let initial = indicator.build_day_profile(&data_source, today, group_step, false);
        indicator.cache = RefCell::new(Some(OverlayCache {
            full_rev: indicator.full_rev,
            group_step_units: group_step.units,
            lookback_days: indicator.lookback_days,
            today,
            qty_is_quote: false,
            dirty_from_day: None,
            days: vec![initial],
        }));

        // Opposite-side volume reduces the previous maximum-delta row, which
        // exercises the precise max rescan as well as the direct bucket update.
        indicator.on_source_trades(source, &[trade(today + 3_000, 100.0, 8.0, true)], false);

        let expected = indicator.build_day_profile(&data_source, today, group_step, false);
        let cache = indicator.cache.borrow();
        let actual = &cache.as_ref().expect("incremental cache").days[0];
        assert_eq!(actual.grouped, expected.grouped);
        assert_eq!(actual.max_abs, expected.max_abs);
        assert_eq!(actual.max_abs_price, expected.max_abs_price);
        assert_eq!(actual.poc, expected.poc);
        assert_eq!(actual.profile_high, expected.profile_high);
        assert_eq!(actual.profile_low, expected.profile_low);
        assert_eq!(cache.as_ref().unwrap().dirty_from_day, None);
    }

    #[test]
    fn kline_correction_retracts_cached_day_extrema_without_regrouping_trades() {
        let source = source();
        let today = day_start(UnixMs::now());
        let step = PriceStep::from(source.min_ticksize);
        let group_step = PriceStep {
            units: Price::from_f64(1.0).units,
        };
        let make_kline = |high: f64, low: f64| Kline {
            time: UnixMs::new(today),
            open: Price::from_f64(100.0),
            high: Price::from_f64(high),
            low: Price::from_f64(low),
            close: Price::from_f64(100.0),
            volume: Volume::empty_buy_sell(),
        };
        let mut data_source = PlotData::TimeBased(TimeSeries::<KlineDataPoint>::new(
            Timeframe::M1,
            step,
            &[make_kline(110.0, 90.0)],
        ));
        let mut indicator = DailyDeltaIndicator::new();
        indicator.configure_footprint_history(&[source], true);
        indicator.on_source_trades(source, &[trade(today + 1_000, 100.0, 10.0, false)], true);
        let initial = indicator.build_day_profile(&data_source, today, group_step, false);
        indicator.cache = RefCell::new(Some(OverlayCache {
            full_rev: indicator.full_rev,
            group_step_units: group_step.units,
            lookback_days: indicator.lookback_days,
            today,
            qty_is_quote: false,
            dirty_from_day: None,
            days: vec![initial],
        }));

        let corrected = make_kline(105.0, 95.0);
        let PlotData::TimeBased(series) = &mut data_source else {
            unreachable!();
        };
        series.insert_klines(&[corrected]);
        indicator.on_insert_klines(&[corrected], &data_source);

        let expected = indicator.build_day_profile(&data_source, today, group_step, false);
        let cache = indicator.cache.borrow();
        let actual = &cache.as_ref().expect("corrected cache").days[0];
        assert_eq!(actual.profile_high, expected.profile_high);
        assert_eq!(actual.profile_low, expected.profile_low);
        assert_eq!(actual.grouped, expected.grouped);
        assert_eq!(cache.as_ref().unwrap().dirty_from_day, None);
    }

    #[test]
    fn subpixel_lod_preserves_visible_delta_exactly() {
        let mut grouped = BTreeMap::new();
        for row in 0..2_400_i64 {
            grouped
                .entry(row * 10)
                .or_insert_with(LevelStats::default)
                .add_notional(row % 3 == 0, 100.0 + row as f64);
        }
        let source_sum = grouped.values().map(|level| level.delta()).sum::<f64>();
        let (render_step, rows) = delta_rows_for_view(&grouped, 10, 6, 0, 23_990);
        let rendered_sum = rows.iter().map(|(_, delta)| *delta).sum::<f64>();

        assert_eq!(render_step, 60);
        assert_eq!(rows.len(), 400);
        assert_eq!(rendered_sum, source_sum);
    }
}
