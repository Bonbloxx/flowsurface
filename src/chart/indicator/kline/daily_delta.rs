use super::KlineIndicatorImpl;
use super::footprint_history::{
    DAY_MS, DayStats, DisplayDay, FootprintHistoryIndicator, day_start, draw_text, group_levels,
    utc_days_covering,
};
use crate::chart::{Message, ViewState};

use data::aggr::ticks::TickAggr;
use data::aggr::time::TimeSeries;
use data::chart::PlotData;
use data::chart::kline::{KlineDataPoint, KlineTrades};
use exchange::UnixMs;
use exchange::unit::{Price, PriceStep};
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Path};
use iced::{Alignment, Color, Element, Point, Rectangle, Size};

/// Daily dollar-delta profile overlay. Reuses Footprint History's UTC-day
/// trade book and venue aggregation; only the chart drawing is new.
pub struct DailyDeltaIndicator {
    inner: FootprintHistoryIndicator,
    lookback_days: usize,
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
    }

    fn set_trade_history_lookback(&mut self, days: u16) {
        self.lookback_days = usize::from(days).clamp(1, 30);
        self.inner.set_lookback_days(days);
    }

    fn reset_trade_history_backfill(&mut self) {
        self.inner.reset_trade_history_backfill();
    }

    fn prepare_footprint_history(&mut self, source: exchange::TickerInfo, cutoff: UnixMs) {
        self.inner.prepare_footprint_history(source, cutoff);
    }

    fn load_cached_footprint_day(
        &mut self,
        source: exchange::TickerInfo,
        day_start: UnixMs,
    ) -> Option<UnixMs> {
        self.inner.load_cached_footprint_day(source, day_start)
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

        let day_starts = display_day_starts(now, self.lookback_days);

        for day_ts in day_starts {
            let mut day = self.inner.display_day_at(day_ts);
            apply_kline_day(&mut day, data_source, day_ts, group_step);
            if day.stats.levels.is_empty() && day.stats.high.is_none() && day.stats.low.is_none() {
                continue;
            }
            let is_current = day_ts == today;
            let profile_w = if is_current { current_w } else { history_w };
            let Some(x) = day_x(
                chart,
                data_source,
                region,
                profile_w,
                pad,
                day_ts,
                is_current,
                now,
            ) else {
                continue;
            };

            draw_day_profile(
                frame, chart, &day, group_step, x, profile_w, highest, lowest, scaling, buy, sell,
                axis, label, is_current,
            );
        }
    }
}

fn day_x(
    chart: &ViewState,
    data_source: &PlotData<KlineDataPoint>,
    region: Rectangle,
    profile_w: f32,
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

    let left = region.x - profile_w;
    let right = region.x + region.width + profile_w;
    (x >= left && x <= right).then_some(x)
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

fn apply_kline_day(
    day: &mut DisplayDay,
    data_source: &PlotData<KlineDataPoint>,
    day_ts: u64,
    step: PriceStep,
) {
    let day_end = day_ts.saturating_add(DAY_MS);
    match data_source {
        PlotData::TimeBased(series) => {
            absorb_time_series(&mut day.stats, series, day_ts, day_end, step);
        }
        PlotData::TickBased(tick_aggr) => {
            for dp in &tick_aggr.datapoints {
                let time = dp.kline.time.as_u64();
                if time >= day_ts && time < day_end {
                    absorb_ohlc(&mut day.stats, dp.kline.high, dp.kline.low);
                    absorb_footprint(&mut day.stats, &dp.footprint, step);
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
) {
    let start = UnixMs::new(day_ts);
    let end = UnixMs::new(day_end);
    for (_, dp) in series.datapoints.range(start..end) {
        absorb_ohlc(stats, dp.kline.high, dp.kline.low);
        absorb_footprint(stats, &dp.footprint, step);
    }
}

fn absorb_ohlc(stats: &mut DayStats, high: Price, low: Price) {
    let high = high.to_f64();
    let low = low.to_f64();
    stats.high = Some(stats.high.map_or(high, |value| value.max(high)));
    stats.low = Some(stats.low.map_or(low, |value| value.min(low)));
}

fn absorb_footprint(stats: &mut DayStats, footprint: &KlineTrades, step: PriceStep) {
    if step.units <= 0 {
        return;
    }
    for (price, group) in &footprint.trades {
        let bucket = price.units.div_euclid(step.units) * step.units;
        let px = price.to_f64();
        let level = stats.levels.entry(bucket).or_default();
        level.add_notional(false, px * group.buy_qty.to_f64());
        level.add_notional(true, px * group.sell_qty.to_f64());
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_day_profile(
    frame: &mut canvas::Frame,
    chart: &ViewState,
    day: &DisplayDay,
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
) {
    let step = group_step;
    let step_u = step.units.max(1);
    let grouped = group_levels(&day.stats, step);
    let max_abs = grouped
        .values()
        .map(|level| level.delta().abs())
        .fold(0.0_f64, f64::max);

    let poc = grouped
        .iter()
        .max_by(|left, right| left.1.volume().total_cmp(&right.1.volume()))
        .map(|(price_units, _)| *price_units);

    let profile_high = day
        .stats
        .high
        .map(|price| Price::from_f64(price).units)
        .or_else(|| grouped.keys().next_back().copied())
        .unwrap_or(highest.units);
    let profile_low = day
        .stats
        .low
        .map(|price| Price::from_f64(price).units)
        .or_else(|| grouped.keys().next().copied())
        .unwrap_or(lowest.units);
    if profile_high < profile_low {
        return;
    }
    let low_u = profile_low.div_euclid(step_u) * step_u;
    let high_u = profile_high.div_euclid(step_u) * step_u;
    let top_y = chart.price_to_y(Price::from_units(high_u.saturating_add(step_u)));
    let bot_y = chart.price_to_y(Price::from_units(low_u));

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

    for (price_units, level) in &grouped {
        let delta = level.delta();
        if delta == 0.0 || max_abs <= 0.0 {
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

    if let Some(poc_units) = poc {
        let poc_y = {
            let price = Price::from_units(poc_units);
            let next = Price::from_units(poc_units.saturating_add(step_u));
            (chart.price_to_y(price) + chart.price_to_y(next)) * 0.5
        };
        frame.fill(
            &Path::circle(Point::new(x + 6.0 / scaling, poc_y), 4.0 / scaling),
            buy,
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
    use exchange::{TickerInfo, Trade, adapter::Exchange, unit::Qty};

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
}
