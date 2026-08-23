use super::KlineIndicatorImpl;
use super::footprint_history::{
    DAY_MS, DayStats, DisplayDay, FootprintHistoryIndicator, LevelStats, day_start, draw_text,
    group_levels, utc_days_covering,
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
}

struct OverlayCache {
    full_rev: u64,
    group_step_units: i64,
    lookback_days: usize,
    today: u64,
    /// Days at/after this UTC-day start are stale and must be rebuilt
    /// before the next draw.
    dirty_from_day: Option<u64>,
    days: Vec<CachedDayProfile>,
}

struct CachedDayProfile {
    day_ts: u64,
    grouped: BTreeMap<i64, LevelStats>,
    max_abs: f64,
    poc: Option<i64>,
    profile_high: Option<i64>,
    profile_low: Option<i64>,
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
        apply_kline_day(&mut day, data_source, day_ts, group_step, qty_is_quote);
        CachedDayProfile {
            day_ts,
            ..build_cached_profile(&day.stats, group_step)
        }
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
        if loaded.is_some() {
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
            self.mark_days_dirty(earliest);
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

    fn on_insert_klines(&mut self, klines: &[exchange::Kline], _source: &PlotData<KlineDataPoint>) {
        if let Some(earliest) = klines.iter().map(|kline| kline.time.as_u64()).min() {
            self.mark_days_dirty(earliest);
        }
    }

    fn on_insert_trades(
        &mut self,
        trades: &[Trade],
        _old_dp_len: usize,
        _source: &PlotData<KlineDataPoint>,
    ) {
        if let Some(earliest) = trades.iter().map(|trade| trade.time.as_u64()).min() {
            self.mark_days_dirty(earliest);
        }
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
            if !profile_intersects_region(region, x, profile_w) && poc_line.is_none() {
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
    let max_abs = grouped
        .values()
        .map(|level| level.delta().abs())
        .fold(0.0_f64, f64::max);
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
        poc,
        profile_high,
        profile_low,
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
        poc,
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

    for (price_units, level) in grouped {
        let delta = level.delta();
        if delta == 0.0 || *max_abs <= 0.0 {
            continue;
        }
        let price = Price::from_units(*price_units);
        let next = Price::from_units(price_units.saturating_add(step_u));
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
        let width = ((delta.abs() / max_abs) as f32 * profile_w).max(4.0 / scaling);
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

    if let (Some(poc_units), Some((line_start, line_end))) = (poc, poc_line) {
        let poc_y = {
            let price = Price::from_units(*poc_units);
            let next = Price::from_units(poc_units.saturating_add(step_u));
            (chart.price_to_y(price) + chart.price_to_y(next)) * 0.5
        };
        frame.stroke(
            &Path::line(Point::new(line_start, poc_y), Point::new(line_end, poc_y)),
            Stroke::with_color(
                Stroke {
                    width: 2.0 / scaling,
                    ..Stroke::default()
                },
                buy,
            ),
        );
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
}
