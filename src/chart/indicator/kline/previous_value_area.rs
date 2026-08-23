use super::KlineIndicatorImpl;
use super::footprint_history::{DAY_MS, draw_text};
use crate::chart::{Message, ViewState};

use chrono::{Datelike, TimeZone, Utc};
use data::aggr::ticks::TickAggr;
use data::chart::PlotData;
use data::chart::kline::KlineDataPoint;
use data::chart::tpo::{self, Config as TpoConfig, Profile as TpoProfile};
use exchange::unit::{Price, PriceStep};
use exchange::{Kline, UnixMs};
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Alignment, Color, Element, Point, Rectangle};

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
    /// Exclusive end of the previous period == start of the current one.
    previous_end: UnixMs,
    current_start: UnixMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValueArea {
    high: Price,
    poc: Price,
    low: Price,
}

/// Previous completed day/week/month/year value-area overlay.
///
/// Profiles are built with the exact TPO machinery (`data::chart::tpo`) from
/// letter-timeframe exchange OHLC bars, so the plotted VAH/VAL/POC match the
/// TPO surface whenever the block size, ticks-per-row, session anchor and
/// value-area percent knobs agree.
pub struct PreviousValueAreaIndicator {
    /// Composite letter-timeframe bars mirrored from the chart's bar store so
    /// period rollovers can rebuild without waiting for new pages.
    bars: Vec<Kline>,
    areas: Vec<(PeriodRange, ValueArea)>,
    built_with: Option<(TpoConfig, PriceStep)>,
    range_key: Vec<(u64, u64)>,
}

impl PreviousValueAreaIndicator {
    pub fn new() -> Self {
        Self {
            bars: Vec::new(),
            areas: Vec::new(),
            built_with: None,
            range_key: Vec::new(),
        }
    }
}

impl KlineIndicatorImpl for PreviousValueAreaIndicator {
    fn clear_all_caches(&mut self) {}

    fn clear_crosshair_caches(&mut self) {}

    fn element<'a>(
        &'a self,
        _chart: &'a ViewState,
        _data_labels_always_visible: bool,
        _visible_range: std::ops::RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        iced::widget::row![].into()
    }

    fn sync_value_areas(
        &mut self,
        bars: Option<&[Kline]>,
        config: TpoConfig,
        row_step: PriceStep,
        now: UnixMs,
    ) -> bool {
        // `Some` means the letter-timeframe store changed. Rebuild even when
        // config and period bounds are unchanged — a first empty tick after
        // restore would otherwise cache `built_with` and ignore later pages.
        let bars_changed = if let Some(bars) = bars {
            self.bars = bars.to_vec();
            true
        } else {
            false
        };

        let ranges = period_ranges(config, now);
        let range_key = ranges
            .iter()
            .map(|period| {
                (
                    period.previous_start.as_u64(),
                    period.current_start.as_u64(),
                )
            })
            .collect::<Vec<_>>();
        let needs_rebuild = bars_changed
            || self.built_with != Some((config, row_step))
            || self.range_key != range_key;
        self.built_with = Some((config, row_step));
        self.range_key = range_key;

        if !needs_rebuild {
            return false;
        }
        self.areas = ranges
            .iter()
            .filter_map(|period| {
                profile_value_area(&self.bars, period, config, row_step).map(|area| (*period, area))
            })
            .collect();
        true
    }

    fn draw_overlay(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        _group_step: PriceStep,
    ) {
        let scaling = chart.scaling.max(0.01);
        let line_end = region.x + region.width;
        for (period, area) in &self.areas {
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
                *area,
                color,
            );
        }
    }
}

/// Earliest bar time the Previous Value Areas bar store must cover.
pub(crate) fn value_area_history_earliest(config: TpoConfig, now: UnixMs) -> UnixMs {
    UnixMs::new(
        period_ranges(config, now)
            .iter()
            .map(|period| period.previous_start.as_u64())
            .min()
            .map(|earliest| earliest.saturating_sub(1))
            .unwrap_or_else(|| {
                now.as_u64()
                    .saturating_sub(u64::from(MAX_LOOKBACK_DAYS) * DAY_MS)
            }),
    )
}

/// Completed previous periods relative to `now`.
///
/// Day and week boundaries follow the TPO session anchoring
/// (`Config::profile_start`) so they line up exactly with TPO profiles.
/// Months and years have no TPO counterpart and use UTC calendar ranges.
fn period_ranges(config: TpoConfig, now: UnixMs) -> Vec<PeriodRange> {
    let mut week_config = config.normalized();
    week_config.profile_period = tpo::ProfilePeriod::Week;
    let day_config = config.normalized();

    let current_day = day_config.profile_start(now);
    let previous_day = day_config.profile_start(current_day.saturating_sub(1));
    let current_week = week_config.profile_start(now);
    let previous_week = week_config.profile_start(current_week.saturating_sub(1));

    let now_dt = Utc
        .timestamp_millis_opt(now.as_u64().min(i64::MAX as u64) as i64)
        .single()
        .unwrap_or_else(Utc::now);
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

    vec![
        make_period(PeriodKind::Day, previous_day, current_day),
        make_period(PeriodKind::Week, previous_week, current_week),
        make_period(
            PeriodKind::Month,
            UnixMs::new(previous_month.timestamp_millis().max(0) as u64),
            UnixMs::new(current_month.timestamp_millis().max(0) as u64),
        ),
        make_period(
            PeriodKind::Year,
            UnixMs::new(previous_year.timestamp_millis().max(0) as u64),
            UnixMs::new(current_year.timestamp_millis().max(0) as u64),
        ),
    ]
}

fn make_period(kind: PeriodKind, previous_start: UnixMs, current_start: UnixMs) -> PeriodRange {
    PeriodRange {
        kind,
        previous_start,
        previous_end: current_start,
        current_start,
    }
}

/// Build the period's TPO profile from letter-timeframe bars and read its
/// value area — the same numbers a TPO pane shows for that session.
fn profile_value_area(
    bars: &[Kline],
    period: &PeriodRange,
    config: TpoConfig,
    row_step: PriceStep,
) -> Option<ValueArea> {
    if row_step.units <= 0 {
        return None;
    }
    let cfg = config.normalized();
    let start_index = bars.partition_point(|bar| bar.time < period.previous_start);
    let end_index = bars.partition_point(|bar| bar.time < period.previous_end);
    let slice = &bars[start_index..end_index];
    let first = slice.first()?;

    let mut profile = TpoProfile::empty_at(cfg, period.previous_start, first.open);
    for bar in slice {
        profile.apply_kline(cfg, row_step, bar);
    }
    profile.recalculate(cfg, row_step);

    (profile.total_tpos > 0).then_some(ValueArea {
        high: profile.value_area_high,
        poc: profile.poc,
        low: profile.value_area_low,
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
    use chrono::Timelike;

    fn step(units: f64) -> PriceStep {
        PriceStep {
            units: Price::from_f64(units).units,
        }
    }

    fn bar(time: u64, open: f64, high: f64, low: f64, close: f64) -> Kline {
        Kline {
            time: UnixMs::new(time),
            open: Price::from_f64(open),
            high: Price::from_f64(high),
            low: Price::from_f64(low),
            close: Price::from_f64(close),
            volume: exchange::Volume::TotalOnly(exchange::unit::Qty::from_f64(1.0)),
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
        let ranges = period_ranges(TpoConfig::default(), now);
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
    fn value_area_matches_expected_tpo_rows() {
        // Two 30m letters inside the previous UTC day. Row counts make
        // rows 102 and 103 the longest; the midpoint tie-break picks POC 102,
        // and the 70% expansion covers rows 101..=104.
        let day = 10 * DAY_MS;
        let bars = vec![
            bar(day - DAY_MS, 101.0, 103.0, 100.0, 102.0),
            bar(day - DAY_MS + 30 * 60_000, 103.0, 104.0, 102.0, 103.5),
        ];
        let now = UnixMs::new(day + 12 * 3_600_000);
        let period = period_ranges(TpoConfig::default(), now).remove(0);
        assert_eq!(period.kind, PeriodKind::Day);

        let area = profile_value_area(&bars, &period, TpoConfig::default(), step(1.0)).unwrap();
        assert_eq!(area.poc, Price::from_f64(102.0));
        assert_eq!(area.low, Price::from_f64(101.0));
        assert_eq!(area.high, Price::from_f64(104.0));
    }

    #[test]
    fn value_areas_rebuild_when_bars_arrive_after_empty_sync() {
        // App restart: the first periodic tick syncs with an empty store, then
        // letter-timeframe pages arrive. Areas must populate without toggling
        // the indicator (which reconstructs it and clears `built_with`).
        let mut indicator = PreviousValueAreaIndicator::new();
        let config = TpoConfig::default();
        let row_step = step(1.0);
        let day = 10 * DAY_MS;
        let now = UnixMs::new(day + 12 * 3_600_000);

        indicator.sync_value_areas(Some(&[]), config, row_step, now);
        assert!(indicator.areas.is_empty());

        let bars = vec![
            bar(day - DAY_MS, 101.0, 103.0, 100.0, 102.0),
            bar(day - DAY_MS + 30 * 60_000, 103.0, 104.0, 102.0, 103.5),
        ];
        indicator.sync_value_areas(Some(&bars), config, row_step, now);
        let day_area = indicator
            .areas
            .iter()
            .find(|(period, _)| period.kind == PeriodKind::Day)
            .map(|(_, area)| *area);
        assert_eq!(day_area.map(|area| area.poc), Some(Price::from_f64(102.0)));

        indicator.sync_value_areas(None, config, row_step, now);
        assert!(
            indicator
                .areas
                .iter()
                .any(|(period, _)| period.kind == PeriodKind::Day)
        );
    }

    #[test]
    fn empty_period_yields_no_value_area() {
        let period = PeriodRange {
            kind: PeriodKind::Day,
            previous_start: UnixMs::new(0),
            previous_end: UnixMs::new(1),
            current_start: UnixMs::new(1),
        };
        assert!(profile_value_area(&[], &period, TpoConfig::default(), step(1.0)).is_none());
    }

    #[test]
    fn session_anchor_shifts_day_boundary_like_tpo() {
        let now = UnixMs::new(
            Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64,
        );
        let config = TpoConfig {
            session_start_minutes_utc: 5 * 60, // 05:00 UTC session start
            ..TpoConfig::default()
        };
        let ranges = period_ranges(config, now);
        let to_hour = |time: UnixMs| {
            Utc.timestamp_millis_opt(time.as_u64() as i64)
                .single()
                .unwrap()
                .hour()
        };
        assert_eq!(to_hour(ranges[0].previous_start), 5);
        assert_eq!(to_hour(ranges[0].current_start), 5);
    }
}
