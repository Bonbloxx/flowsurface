use super::KlineIndicatorImpl;
use super::footprint_history::{
    DAY_MS, DayStats, FootprintHistoryIndicator, draw_text, group_levels,
};
use crate::chart::{Message, ViewState};

use chrono::{Datelike, Duration, TimeZone, Utc};
use data::aggr::ticks::TickAggr;
use data::chart::PlotData;
use data::chart::kline::KlineDataPoint;
use exchange::UnixMs;
use exchange::unit::{Price, PriceStep};
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Alignment, Color, Element, Point, Rectangle};
use std::cmp::Ordering;

const VALUE_AREA_FRACTION: f64 = 0.70;
const MAX_LOOKBACK_DAYS: u16 = 732;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeriodKind {
    Day,
    Week,
    Month,
    Year,
}

impl PeriodKind {
    fn short_name(self) -> &'static str {
        match self {
            Self::Day => "PWD",
            Self::Week => "PW",
            Self::Month => "PM",
            Self::Year => "PY",
        }
    }

    fn includes_poc(self) -> bool {
        !matches!(self, Self::Day)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeriodRange {
    kind: PeriodKind,
    previous_start: UnixMs,
    previous_end: UnixMs,
    current_start: UnixMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValueArea {
    high: Price,
    poc: Price,
    low: Price,
}

/// Previous completed UTC day/week/month/year value-area overlay.
///
/// The underlying executed-volume books, source selection, aggregation and
/// disk cache are all provided by Footprint History.
pub struct PreviousValueAreaIndicator {
    inner: FootprintHistoryIndicator,
    profile_step: PriceStep,
}

impl PreviousValueAreaIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(required_lookback_days(UnixMs::now()));
        Self {
            inner,
            profile_step: PriceStep { units: 1 },
        }
    }

    fn compact_completed_day(&mut self, source: exchange::TickerInfo, day_start: UnixMs) {
        self.inner
            .compact_day_for_value_area(source, day_start, self.profile_step);
    }
}

impl KlineIndicatorImpl for PreviousValueAreaIndicator {
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
        self.inner
            .set_lookback_days(required_lookback_days(UnixMs::now()));
    }

    fn set_trade_history_lookback(&mut self, _days: u16) {
        self.inner
            .set_lookback_days(required_lookback_days(UnixMs::now()));
    }

    fn set_trade_history_price_step(&mut self, step: PriceStep) {
        self.profile_step = PriceStep {
            units: step.units.max(1),
        };
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
        let loaded = self.inner.load_cached_footprint_day(source, day_start);
        if loaded.is_some() {
            self.compact_completed_day(source, day_start);
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
        self.compact_completed_day(source, day_start);
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

        let scaling = chart.scaling.max(0.01);
        let line_end = region.x + region.width;
        for period in period_ranges(UnixMs::now()) {
            let (stats, complete_sources) = self
                .inner
                .display_complete_period(period.previous_start, period.previous_end);
            if complete_sources == 0 {
                continue;
            }
            let Some(area) = calculate_value_area(&stats, group_step) else {
                continue;
            };
            let Some(line_start) = period_start_x(chart, data_source, region, period.current_start)
            else {
                continue;
            };
            let color = period_color(period.kind, palette);
            draw_period_levels(
                frame,
                chart,
                region,
                scaling,
                line_start,
                line_end,
                period.kind,
                area,
                color,
            );
        }
    }
}

pub(crate) fn trade_history_ranges(now: UnixMs) -> Vec<(UnixMs, UnixMs)> {
    period_ranges(now)
        .into_iter()
        .map(|period| (period.previous_start, period.previous_end))
        .collect()
}

pub(crate) fn required_lookback_days(now: UnixMs) -> u16 {
    let earliest = period_ranges(now)
        .into_iter()
        .map(|period| period.previous_start.as_u64())
        .min()
        .unwrap_or(now.as_u64());
    let days = now
        .as_u64()
        .saturating_sub(earliest)
        .div_euclid(DAY_MS)
        .saturating_add(1);
    u16::try_from(days)
        .unwrap_or(MAX_LOOKBACK_DAYS)
        .min(MAX_LOOKBACK_DAYS)
}

fn period_ranges(now: UnixMs) -> [PeriodRange; 4] {
    let now_dt = Utc
        .timestamp_millis_opt(now.as_u64().min(i64::MAX as u64) as i64)
        .single()
        .unwrap_or_else(Utc::now);
    let today = now_dt
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid UTC time")
        .and_utc();
    let previous_day = today - Duration::days(1);

    let current_week = today - Duration::days(i64::from(now_dt.weekday().num_days_from_monday()));
    let previous_week = current_week - Duration::days(7);

    let current_month = Utc
        .with_ymd_and_hms(now_dt.year(), now_dt.month(), 1, 0, 0, 0)
        .single()
        .expect("current UTC month start is valid");
    let (previous_month_year, previous_month_number) = if now_dt.month() == 1 {
        (now_dt.year() - 1, 12)
    } else {
        (now_dt.year(), now_dt.month() - 1)
    };
    let previous_month = Utc
        .with_ymd_and_hms(previous_month_year, previous_month_number, 1, 0, 0, 0)
        .single()
        .expect("previous UTC month start is valid");

    let current_year = Utc
        .with_ymd_and_hms(now_dt.year(), 1, 1, 0, 0, 0)
        .single()
        .expect("current UTC year start is valid");
    let previous_year = Utc
        .with_ymd_and_hms(now_dt.year() - 1, 1, 1, 0, 0, 0)
        .single()
        .expect("previous UTC year start is valid");

    [
        make_period(PeriodKind::Day, previous_day, today),
        make_period(PeriodKind::Week, previous_week, current_week),
        make_period(PeriodKind::Month, previous_month, current_month),
        make_period(PeriodKind::Year, previous_year, current_year),
    ]
}

fn make_period(
    kind: PeriodKind,
    previous_start: chrono::DateTime<Utc>,
    current_start: chrono::DateTime<Utc>,
) -> PeriodRange {
    PeriodRange {
        kind,
        previous_start: UnixMs::new(previous_start.timestamp_millis().max(0) as u64),
        previous_end: UnixMs::new(current_start.timestamp_millis().max(0).saturating_sub(1) as u64),
        current_start: UnixMs::new(current_start.timestamp_millis().max(0) as u64),
    }
}

fn calculate_value_area(stats: &DayStats, step: PriceStep) -> Option<ValueArea> {
    if step.units <= 0 {
        return None;
    }
    let grouped = group_levels(stats, step);
    if grouped.is_empty() {
        return None;
    }

    let prices = grouped.keys().copied().collect::<Vec<_>>();
    let volumes = grouped
        .values()
        .map(|level| level.volume().max(0.0))
        .collect::<Vec<_>>();
    let total = volumes.iter().sum::<f64>();
    if !total.is_finite() || total <= 0.0 {
        return None;
    }

    let midpoint = (i128::from(*prices.first()?) + i128::from(*prices.last()?)) / 2;
    let mut poc_index = 0;
    for index in 1..prices.len() {
        let order = volumes[index].total_cmp(&volumes[poc_index]);
        let distance = |price: i64| (i128::from(price) - midpoint).abs();
        if order == Ordering::Greater
            || (order == Ordering::Equal
                && (distance(prices[index]) < distance(prices[poc_index])
                    || (distance(prices[index]) == distance(prices[poc_index])
                        && prices[index] < prices[poc_index])))
        {
            poc_index = index;
        }
    }

    let target = total * VALUE_AREA_FRACTION;
    let mut included = volumes[poc_index];
    let mut low_index = poc_index;
    let mut high_index = poc_index;
    while included < target && (low_index > 0 || high_index + 1 < prices.len()) {
        let below = low_index.checked_sub(1);
        let above = (high_index + 1 < prices.len()).then_some(high_index + 1);
        match (below, above) {
            (Some(below), Some(above)) => match volumes[above].total_cmp(&volumes[below]) {
                Ordering::Greater => {
                    high_index = above;
                    included += volumes[above];
                }
                Ordering::Less => {
                    low_index = below;
                    included += volumes[below];
                }
                Ordering::Equal => {
                    low_index = below;
                    high_index = above;
                    included += volumes[below] + volumes[above];
                }
            },
            (Some(below), None) => {
                low_index = below;
                included += volumes[below];
            }
            (None, Some(above)) => {
                high_index = above;
                included += volumes[above];
            }
            (None, None) => break,
        }
    }

    Some(ValueArea {
        high: Price::from_units(prices[high_index]),
        poc: Price::from_units(prices[poc_index]),
        low: Price::from_units(prices[low_index]),
    })
}

fn period_start_x(
    chart: &ViewState,
    data_source: &PlotData<KlineDataPoint>,
    region: Rectangle,
    current_start: UnixMs,
) -> Option<f32> {
    let right = region.x + region.width;
    let x = match data_source {
        PlotData::TimeBased(_) => {
            let (_, visible_latest) = chart.interval_range(&region);
            if visible_latest < current_start.as_u64() {
                return None;
            }
            chart.interval_to_x(current_start.as_u64())
        }
        PlotData::TickBased(ticks) => tick_period_start_x(chart, ticks, current_start)?,
    };
    (x <= right).then_some(x.max(region.x))
}

fn tick_period_start_x(chart: &ViewState, ticks: &TickAggr, current_start: UnixMs) -> Option<f32> {
    let oldest = ticks.datapoints.first()?.kline.time;
    let newest = ticks.datapoints.last()?.kline.time;
    if newest < current_start {
        return None;
    }
    if oldest >= current_start {
        return Some(f32::NEG_INFINITY);
    }
    let len = ticks.datapoints.len();
    let index = ticks
        .datapoints
        .iter()
        .position(|point| point.kline.time >= current_start)?;
    Some(chart.interval_to_x((len - 1 - index) as u64))
}

fn period_color(kind: PeriodKind, palette: &Extended) -> Color {
    match kind {
        PeriodKind::Day => palette.secondary.base.color,
        PeriodKind::Week => palette.primary.base.color,
        PeriodKind::Month => palette.warning.base.color,
        PeriodKind::Year => palette.success.base.color,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_period_levels(
    frame: &mut canvas::Frame,
    chart: &ViewState,
    region: Rectangle,
    scaling: f32,
    line_start: f32,
    line_end: f32,
    kind: PeriodKind,
    area: ValueArea,
    color: Color,
) {
    let text_size = (9.0 / scaling).clamp(7.0, 11.0);
    let label_x = line_end - 4.0 / scaling;
    let levels = [
        Some((area.high, "VAH", false)),
        kind.includes_poc().then_some((area.poc, "POC", true)),
        Some((area.low, "VAL", false)),
    ];
    for (price, suffix, strong) in levels.into_iter().flatten() {
        let y = chart.price_to_y(price);
        if y < region.y || y > region.y + region.height {
            continue;
        }
        let ink = if strong {
            color
        } else {
            color.scale_alpha(0.72)
        };
        frame.stroke(
            &Path::line(Point::new(line_start, y), Point::new(line_end, y)),
            Stroke::with_color(
                Stroke {
                    width: if strong { 2.0 / scaling } else { 1.0 / scaling },
                    ..Stroke::default()
                },
                ink,
            ),
        );
        draw_text(
            frame,
            &format!("{} {suffix}", kind.short_name()),
            Point::new(label_x, y - 6.0 / scaling),
            text_size,
            ink,
            Alignment::End,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{TickerInfo, Trade, adapter::Exchange, unit::Qty};

    fn stats(levels: &[(f64, f64)]) -> DayStats {
        let mut stats = DayStats::default();
        for (price, volume) in levels {
            stats
                .levels
                .entry(Price::from_f64(*price).units)
                .or_default()
                .add_notional(false, *volume);
        }
        stats
    }

    fn source(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(exchange::Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    fn trade(time: UnixMs, price: f64, qty: f64) -> Trade {
        Trade {
            time,
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell: false,
        }
    }

    #[test]
    fn previous_periods_use_completed_utc_calendar_ranges() {
        let now = UnixMs::new(
            Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64,
        );
        let ranges = period_ranges(now);
        let to_date = |time: UnixMs| {
            Utc.timestamp_millis_opt(time.as_u64() as i64)
                .single()
                .unwrap()
                .date_naive()
        };

        assert_eq!(
            to_date(ranges[0].previous_start),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 19).unwrap()
        );
        assert_eq!(
            to_date(ranges[0].current_start),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap()
        );
        assert_eq!(
            to_date(ranges[1].previous_start),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()
        );
        assert_eq!(
            to_date(ranges[1].current_start),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()
        );
        assert_eq!(
            to_date(ranges[2].previous_start),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
        );
        assert_eq!(
            to_date(ranges[3].previous_start),
            chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()
        );
    }

    #[test]
    fn previous_day_uses_pwd_label_without_poc() {
        assert_eq!(PeriodKind::Day.short_name(), "PWD");
        assert!(!PeriodKind::Day.includes_poc());
        assert!(PeriodKind::Week.includes_poc());
    }

    #[test]
    fn value_area_expands_from_poc_toward_more_volume() {
        let stats = stats(&[
            (100.0, 10.0),
            (101.0, 20.0),
            (102.0, 50.0),
            (103.0, 15.0),
            (104.0, 5.0),
        ]);
        let area = calculate_value_area(
            &stats,
            PriceStep {
                units: Price::from_f64(1.0).units,
            },
        )
        .unwrap();
        assert_eq!(area.poc, Price::from_f64(102.0));
        assert_eq!(area.low, Price::from_f64(101.0));
        assert_eq!(area.high, Price::from_f64(102.0));
    }

    #[test]
    fn lookback_covers_the_entire_previous_year() {
        let now = UnixMs::new(
            Utc.with_ymd_and_hms(2026, 12, 31, 12, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64,
        );
        assert_eq!(required_lookback_days(now), 730);
    }

    #[test]
    fn aggregate_profile_merges_binance_bybit_and_hyperliquid() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let period = period_ranges(UnixMs::now())[1];
        let time = period.previous_start.saturating_add(3_600_000);
        let mut indicator = PreviousValueAreaIndicator::new();
        indicator
            .inner
            .configure_footprint_history(&[binance, bybit, hyperliquid], true);
        indicator
            .inner
            .on_source_trades(binance, &[trade(time, 100.0, 1.0)], true);
        indicator
            .inner
            .on_source_trades(bybit, &[trade(time, 101.0, 5.0)], true);
        indicator
            .inner
            .on_source_trades(hyperliquid, &[trade(time, 102.0, 10.0)], true);

        let aggregate = indicator
            .inner
            .display_period(period.previous_start, period.previous_end);
        let step = PriceStep {
            units: Price::from_f64(1.0).units,
        };
        assert_eq!(
            calculate_value_area(&aggregate, step).unwrap().poc,
            Price::from_f64(102.0)
        );

        indicator
            .inner
            .configure_footprint_history(&[binance, bybit, hyperliquid], false);
        let single_venue = indicator
            .inner
            .display_period(period.previous_start, period.previous_end);
        assert_eq!(
            calculate_value_area(&single_venue, step).unwrap().poc,
            Price::from_f64(100.0)
        );
    }
}
