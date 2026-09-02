use super::KlineIndicatorImpl;
use super::footprint_history::{DAY_MS, draw_text};
use crate::chart::{Message, ViewState};

use chrono::{DateTime, Datelike, TimeZone, Utc};
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

struct DrawLevel {
    line_start: f32,
    price: Price,
    label: String,
    strong: bool,
    color: Color,
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
    range_key: Vec<(u64, u64, bool)>,
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
        complete_ranges: &[(UnixMs, UnixMs)],
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
                    complete_ranges.contains(&(period.previous_start, period.previous_end)),
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
            .filter(|period| {
                complete_ranges.contains(&(period.previous_start, period.previous_end))
            })
            .filter_map(|period| {
                profile_value_area(&self.bars, period, config, row_step).map(|area| (*period, area))
            })
            .collect();
        log::debug!(
            "history_load kind=previous_value_area bars={} areas={} earliest={:?} latest={:?}",
            self.bars.len(),
            self.areas.len(),
            self.bars.first().map(|bar| bar.time),
            self.bars.last().map(|bar| bar.time),
        );
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
        let mut levels = Vec::new();
        for (period, area) in &self.areas {
            let Some(line_start) = period_start_x(chart, data_source, region, period.current_start)
            else {
                continue;
            };
            append_period_levels(
                &mut levels,
                line_start,
                period.kind,
                *area,
                period_color(period.kind, palette),
            );
        }
        draw_levels(frame, chart, region, scaling, line_end, &levels);
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

pub(crate) fn value_area_period_ranges(config: TpoConfig, now: UnixMs) -> Vec<(UnixMs, UnixMs)> {
    period_ranges(config, now)
        .into_iter()
        .map(|period| (period.previous_start, period.previous_end))
        .collect()
}

/// Completed previous periods relative to `now`.
///
/// Day and week boundaries follow the TPO profile alignment. Month and year
/// boundaries use the same UTC session anchor on their calendar rollover.
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
    let session_anchor = config.normalized().session_start_minutes_utc;
    let current_month = anchored_month_start(now_dt, session_anchor);
    let previous_month = previous_month_start(current_month, session_anchor);
    let current_year = anchored_year_start(now_dt, session_anchor);
    let previous_year = calendar_start(current_year.year() - 1, 1, 1, session_anchor);

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

fn calendar_start(year: i32, month: u32, day: u32, anchor_minutes: u16) -> DateTime<Utc> {
    let hour = u32::from(anchor_minutes / 60);
    let minute = u32::from(anchor_minutes % 60);
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("UTC calendar session start is valid")
}

fn anchored_month_start(now: DateTime<Utc>, anchor_minutes: u16) -> DateTime<Utc> {
    let candidate = calendar_start(now.year(), now.month(), 1, anchor_minutes);
    if now >= candidate {
        candidate
    } else {
        previous_month_start(candidate, anchor_minutes)
    }
}

fn previous_month_start(current: DateTime<Utc>, anchor_minutes: u16) -> DateTime<Utc> {
    let (year, month) = if current.month() == 1 {
        (current.year() - 1, 12)
    } else {
        (current.year(), current.month() - 1)
    };
    calendar_start(year, month, 1, anchor_minutes)
}

fn anchored_year_start(now: DateTime<Utc>, anchor_minutes: u16) -> DateTime<Utc> {
    let candidate = calendar_start(now.year(), 1, 1, anchor_minutes);
    if now >= candidate {
        candidate
    } else {
        calendar_start(now.year() - 1, 1, 1, anchor_minutes)
    }
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

fn append_period_levels(
    levels: &mut Vec<DrawLevel>,
    line_start: f32,
    kind: PeriodKind,
    area: ValueArea,
    color: Color,
) {
    let period_levels = [
        Some((area.high, "VAH", false)),
        kind.includes_poc().then_some((area.poc, "POC", true)),
        Some((area.low, "VAL", false)),
    ];
    levels.extend(
        period_levels
            .into_iter()
            .flatten()
            .map(|(price, suffix, strong)| DrawLevel {
                line_start,
                price,
                label: format!("{} {suffix}", kind.short_name()),
                strong,
                color,
            }),
    );
}

fn draw_levels(
    frame: &mut canvas::Frame,
    chart: &ViewState,
    region: Rectangle,
    scaling: f32,
    line_end: f32,
    levels: &[DrawLevel],
) {
    let text_size = (9.0 / scaling).clamp(7.0, 11.0);
    let visible = levels
        .iter()
        .filter_map(|level| {
            let y = chart.price_to_y(level.price);
            (y >= region.y && y <= region.y + region.height).then_some((level, y))
        })
        .collect::<Vec<_>>();
    let slots = collision_slots(
        &visible.iter().map(|(_, y)| *y).collect::<Vec<_>>(),
        text_size + 2.0 / scaling,
    );

    for ((level, y), slot) in visible.into_iter().zip(slots) {
        let ink = if level.strong {
            level.color
        } else {
            level.color.scale_alpha(0.72)
        };
        frame.stroke(
            &Path::line(Point::new(level.line_start, y), Point::new(line_end, y)),
            Stroke::with_color(
                Stroke {
                    width: if level.strong {
                        2.0 / scaling
                    } else {
                        1.0 / scaling
                    },
                    ..Stroke::default()
                },
                ink,
            ),
        );
        draw_text(
            frame,
            &level.label,
            Point::new(
                line_end - (4.0 + slot as f32 * 58.0) / scaling,
                y - 6.0 / scaling,
            ),
            text_size,
            ink,
            Alignment::End,
        );
    }
}

fn collision_slots(ys: &[f32], minimum_gap: f32) -> Vec<usize> {
    let mut occupied = Vec::<Vec<f32>>::new();
    let mut assignments = Vec::with_capacity(ys.len());
    let minimum_gap = minimum_gap.max(0.0);

    for &y in ys {
        let slot = occupied
            .iter()
            .position(|slot| slot.iter().all(|other| (other - y).abs() >= minimum_gap))
            .unwrap_or_else(|| {
                occupied.push(Vec::new());
                occupied.len() - 1
            });
        occupied[slot].push(y);
        assignments.push(slot);
    }

    assignments
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
        let day_period = period_ranges(config, now).remove(0);
        let complete_day = [(day_period.previous_start, day_period.previous_end)];

        indicator.sync_value_areas(Some(&[]), &complete_day, config, row_step, now);
        assert!(indicator.areas.is_empty());

        let bars = vec![
            bar(day - DAY_MS, 101.0, 103.0, 100.0, 102.0),
            bar(day - DAY_MS + 30 * 60_000, 103.0, 104.0, 102.0, 103.5),
        ];
        indicator.sync_value_areas(Some(&bars), &complete_day, config, row_step, now);
        let day_area = indicator
            .areas
            .iter()
            .find(|(period, _)| period.kind == PeriodKind::Day)
            .map(|(_, area)| *area);
        assert_eq!(day_area.map(|area| area.poc), Some(Price::from_f64(102.0)));

        indicator.sync_value_areas(None, &complete_day, config, row_step, now);
        assert!(
            indicator
                .areas
                .iter()
                .any(|(period, _)| period.kind == PeriodKind::Day)
        );
    }

    #[test]
    fn incomplete_period_stays_hidden_until_coverage_is_complete() {
        let mut indicator = PreviousValueAreaIndicator::new();
        let config = TpoConfig::default();
        let row_step = step(1.0);
        let day = 10 * DAY_MS;
        let now = UnixMs::new(day + 12 * 3_600_000);
        let period = period_ranges(config, now).remove(0);
        let bars = vec![
            bar(day - DAY_MS, 101.0, 103.0, 100.0, 102.0),
            bar(day - DAY_MS + 30 * 60_000, 103.0, 104.0, 102.0, 103.5),
        ];

        indicator.sync_value_areas(Some(&bars), &[], config, row_step, now);
        assert!(indicator.areas.is_empty());

        indicator.sync_value_areas(
            None,
            &[(period.previous_start, period.previous_end)],
            config,
            row_step,
            now,
        );
        assert_eq!(indicator.areas.len(), 1);
        assert_eq!(indicator.areas[0].0.kind, PeriodKind::Day);
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
    fn session_anchor_shifts_every_period_boundary() {
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
        let to_date_time = |time: UnixMs| {
            Utc.timestamp_millis_opt(time.as_u64() as i64)
                .single()
                .unwrap()
        };
        for range in &ranges {
            assert_eq!(to_date_time(range.previous_start).hour(), 5);
            assert_eq!(to_date_time(range.current_start).hour(), 5);
        }
        assert_eq!(
            to_date_time(ranges[1].current_start).weekday(),
            chrono::Weekday::Mon
        );
        assert_eq!(to_date_time(ranges[2].previous_start).month(), 7);
        assert_eq!(to_date_time(ranges[2].current_start).month(), 8);
        assert_eq!(to_date_time(ranges[3].previous_start).year(), 2025);
        assert_eq!(to_date_time(ranges[3].current_start).year(), 2026);
    }

    #[test]
    fn month_and_year_roll_over_only_at_the_session_anchor() {
        let now = UnixMs::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 2, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64,
        );
        let config = TpoConfig {
            session_start_minutes_utc: 5 * 60,
            ..TpoConfig::default()
        };
        let ranges = period_ranges(config, now);
        let to_date_time = |time: UnixMs| {
            Utc.timestamp_millis_opt(time.as_u64() as i64)
                .single()
                .unwrap()
        };

        let current_month = to_date_time(ranges[2].current_start);
        assert_eq!((current_month.year(), current_month.month()), (2025, 12));
        assert_eq!(current_month.hour(), 5);
        let current_year = to_date_time(ranges[3].current_start);
        assert_eq!((current_year.year(), current_year.month()), (2025, 1));
        assert_eq!(current_year.hour(), 5);
    }

    #[test]
    fn colliding_labels_receive_distinct_horizontal_slots() {
        assert_eq!(
            collision_slots(&[100.0, 100.0, 80.0, 85.0], 10.0),
            vec![0, 1, 0, 1]
        );
    }
}
