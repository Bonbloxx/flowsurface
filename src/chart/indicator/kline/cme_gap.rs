use super::KlineIndicatorImpl;
use crate::chart::{Message, ViewState};

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use data::aggr::time::TimeSeries;
use data::chart::PlotData;
use data::chart::kline::KlineDataPoint;
use exchange::unit::{Price, PriceStep};
use exchange::{Kline, Timeframe, UnixMs};
use iced::theme::palette::Extended;
use iced::widget::canvas;
use iced::{Element, Point, Rectangle, Size};

const DAY_MS: u64 = 86_400_000;
const CME_GAP_ALPHA: f32 = 0.14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionWindow {
    close_at: UnixMs,
    reopen_at: UnixMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CmeGap {
    close_at: UnixMs,
    reopen_at: UnixMs,
    low: Price,
    high: Price,
    direction: GapDirection,
    filled_at: Option<UnixMs>,
}

/// Historical CME Bitcoin weekend gaps projected from the chart's existing
/// crypto OHLC source. No second market-data source is opened: the last local
/// close before CME shuts and the first local open after CME resumes define
/// the band.
pub struct CmeGapIndicator {
    gaps: Vec<CmeGap>,
    timeframe: Option<Timeframe>,
    initialized: bool,
}

impl CmeGapIndicator {
    pub fn new() -> Self {
        Self {
            gaps: Vec::new(),
            timeframe: None,
            initialized: false,
        }
    }

    fn rebuild(&mut self, source: &PlotData<KlineDataPoint>) {
        self.gaps.clear();
        self.initialized = true;

        let PlotData::TimeBased(series) = source else {
            self.timeframe = None;
            return;
        };
        self.timeframe = Some(series.interval);
        if !supports_session_edges(series.interval) {
            return;
        }
        let Some((&earliest, _)) = series.datapoints.first_key_value() else {
            return;
        };
        let Some((&latest, _)) = series.datapoints.last_key_value() else {
            return;
        };

        self.gaps = legacy_weekend_windows(earliest, latest)
            .into_iter()
            .filter_map(|window| build_gap(series, window))
            .collect();
    }

    fn refresh_inserted_range(&mut self, inserted: &[Kline], source: &PlotData<KlineDataPoint>) {
        let PlotData::TimeBased(series) = source else {
            self.rebuild(source);
            return;
        };
        if !self.initialized || self.timeframe != Some(series.interval) {
            self.rebuild(source);
            return;
        }
        if !supports_session_edges(series.interval) || inserted.is_empty() {
            return;
        }

        let Some(batch_earliest) = inserted.iter().map(|bar| bar.time).min() else {
            return;
        };
        let Some(batch_latest) = inserted.iter().map(|bar| bar.time).max() else {
            return;
        };

        // A newly inserted/corrected page can establish a nearby weekend edge
        // or move the first fill of an older still-active band. Re-evaluate only
        // those gaps instead of rescanning a multi-year minute series.
        for gap in &mut self.gaps {
            let active_end = gap.filled_at.unwrap_or(UnixMs::new(u64::MAX));
            if batch_latest >= gap.reopen_at && batch_earliest <= active_end {
                gap.filled_at = first_fill(series, gap);
            }
        }

        let window_start = batch_earliest.saturating_sub(3 * DAY_MS);
        let window_end = batch_latest.saturating_add(3 * DAY_MS);
        for window in legacy_weekend_windows(window_start, window_end) {
            let replacement = build_gap(series, window);
            if let Some(index) = self
                .gaps
                .iter()
                .position(|gap| gap.reopen_at == window.reopen_at)
            {
                if let Some(gap) = replacement {
                    self.gaps[index] = gap;
                } else {
                    self.gaps.remove(index);
                }
            } else if let Some(gap) = replacement {
                self.gaps.push(gap);
            }
        }
        self.gaps.sort_unstable_by_key(|gap| gap.reopen_at);
    }
}

impl KlineIndicatorImpl for CmeGapIndicator {
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

    fn rebuild_from_source(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild(source);
    }

    fn on_insert_klines(&mut self, klines: &[Kline], source: &PlotData<KlineDataPoint>) {
        self.refresh_inserted_range(klines, source);
    }

    fn on_basis_change(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild(source);
    }

    fn draw_overlay(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        _data_source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        _group_step: PriceStep,
    ) {
        let Some(timeframe) = self.timeframe else {
            return;
        };
        let interval = timeframe.to_milliseconds();
        let color = palette.warning.base.color.scale_alpha(CME_GAP_ALPHA);
        let visible_right = region.x + region.width;
        let visible_bottom = region.y + region.height;

        for gap in &self.gaps {
            let left = chart.interval_to_x(gap.close_at.as_u64()).max(region.x);
            let right = gap.filled_at.map_or(visible_right, |filled_at| {
                chart
                    .interval_to_x(filled_at.as_u64().saturating_add(interval / 2))
                    .min(visible_right)
            });
            if !left.is_finite() || !right.is_finite() || right <= left {
                continue;
            }

            let high_y = chart.price_to_y(gap.high);
            let low_y = chart.price_to_y(gap.low);
            let top = high_y.min(low_y).max(region.y);
            let bottom = high_y.max(low_y).min(visible_bottom);
            if !top.is_finite() || !bottom.is_finite() || bottom <= top {
                continue;
            }

            frame.fill_rectangle(
                Point::new(left, top),
                Size::new(right - left, bottom - top),
                color,
            );
        }
    }
}

fn supports_session_edges(timeframe: Timeframe) -> bool {
    timeframe.to_milliseconds() <= Timeframe::H1.to_milliseconds()
}

fn build_gap(series: &TimeSeries<KlineDataPoint>, window: SessionWindow) -> Option<CmeGap> {
    let tolerance = series.interval.to_milliseconds();
    let (&close_bar_at, close_point) = series.datapoints.range(..window.close_at).next_back()?;
    if window
        .close_at
        .as_u64()
        .saturating_sub(close_bar_at.as_u64())
        > tolerance
    {
        return None;
    }
    let (&reopen_bar_at, reopen_point) = series.datapoints.range(window.reopen_at..).next()?;
    if reopen_bar_at
        .as_u64()
        .saturating_sub(window.reopen_at.as_u64())
        >= tolerance
    {
        return None;
    }

    let close = close_point.kline.close;
    let reopen = reopen_point.kline.open;
    let (low, high, direction) = if reopen > close {
        (close, reopen, GapDirection::Up)
    } else if reopen < close {
        (reopen, close, GapDirection::Down)
    } else {
        return None;
    };
    let mut gap = CmeGap {
        close_at: window.close_at,
        reopen_at: reopen_bar_at,
        low,
        high,
        direction,
        filled_at: None,
    };
    gap.filled_at = first_fill(series, &gap);
    Some(gap)
}

fn first_fill(series: &TimeSeries<KlineDataPoint>, gap: &CmeGap) -> Option<UnixMs> {
    series
        .datapoints
        .range(gap.reopen_at..)
        .find_map(|(&at, point)| match gap.direction {
            GapDirection::Up if point.kline.low <= gap.low => Some(at),
            GapDirection::Down if point.kline.high >= gap.high => Some(at),
            _ => None,
        })
}

fn legacy_weekend_windows(earliest: UnixMs, latest: UnixMs) -> Vec<SessionWindow> {
    if latest < earliest {
        return Vec::new();
    }
    let Some(mut date) = utc_date(earliest.saturating_sub(3 * DAY_MS)) else {
        return Vec::new();
    };
    let Some(end_date) = utc_date(latest.saturating_add(3 * DAY_MS)) else {
        return Vec::new();
    };
    let cutover = legacy_crypto_schedule_cutover();
    let mut windows = Vec::new();

    while date <= end_date {
        if date.weekday() == Weekday::Fri {
            let reopen_date = date + Duration::days(2);
            let close_at = central_local_to_utc(date, 16);
            let reopen_at = central_local_to_utc(reopen_date, 17);
            if close_at < cutover && reopen_at >= earliest && close_at <= latest {
                windows.push(SessionWindow {
                    close_at,
                    reopen_at,
                });
            }
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    windows
}

/// CME cryptocurrency products moved from the legacy weekday schedule to
/// seven-day trading on Friday, May 29, 2026. Do not manufacture familiar
/// weekend boxes after the underlying CME closure ceased to exist.
fn legacy_crypto_schedule_cutover() -> UnixMs {
    let date = NaiveDate::from_ymd_opt(2026, 5, 29).expect("valid CME cutover date");
    central_local_to_utc(date, 16)
}

fn utc_date(timestamp: UnixMs) -> Option<NaiveDate> {
    Utc.timestamp_millis_opt(timestamp.as_u64().min(i64::MAX as u64) as i64)
        .single()
        .map(|time| time.date_naive())
}

fn central_local_to_utc(date: NaiveDate, hour: u32) -> UnixMs {
    let hours_to_utc = if central_is_daylight_time(date) { 5 } else { 6 };
    let local = date
        .and_hms_opt(hour, 0, 0)
        .expect("CME session hour is valid");
    UnixMs::new(
        (local + Duration::hours(hours_to_utc))
            .and_utc()
            .timestamp_millis()
            .max(0) as u64,
    )
}

fn central_is_daylight_time(date: NaiveDate) -> bool {
    let year = date.year();
    let start = nth_weekday_of_month(year, 3, Weekday::Sun, 2);
    let end = nth_weekday_of_month(year, 11, Weekday::Sun, 1);
    date >= start && date < end
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, nth: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid calendar month");
    let days = (7 + weekday.num_days_from_monday() as i64
        - first.weekday().num_days_from_monday() as i64)
        % 7
        + 7 * i64::from(nth.saturating_sub(1));
    first + Duration::days(days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use data::chart::kline::KlineTrades;
    use exchange::Volume;
    use exchange::unit::Qty;
    use std::collections::BTreeMap;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> UnixMs {
        UnixMs::new(
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64,
        )
    }

    fn point(time: UnixMs, open: f64, high: f64, low: f64, close: f64) -> KlineDataPoint {
        KlineDataPoint {
            kline: Kline {
                time,
                open: Price::from_f64(open),
                high: Price::from_f64(high),
                low: Price::from_f64(low),
                close: Price::from_f64(close),
                volume: Volume::TotalOnly(Qty::ZERO),
            },
            footprint: KlineTrades::default(),
        }
    }

    fn series(points: Vec<KlineDataPoint>) -> TimeSeries<KlineDataPoint> {
        TimeSeries {
            datapoints: points
                .into_iter()
                .map(|point| (point.kline.time, point))
                .collect::<BTreeMap<_, _>>(),
            interval: Timeframe::M1,
            tick_size: PriceStep {
                units: Price::from_f64(1.0).units,
            },
        }
    }

    #[test]
    fn central_session_edges_follow_dst() {
        assert_eq!(
            central_local_to_utc(NaiveDate::from_ymd_opt(2025, 1, 10).unwrap(), 16),
            at(2025, 1, 10, 22, 0)
        );
        assert_eq!(
            central_local_to_utc(NaiveDate::from_ymd_opt(2025, 7, 11).unwrap(), 16),
            at(2025, 7, 11, 21, 0)
        );
        assert_eq!(
            central_local_to_utc(NaiveDate::from_ymd_opt(2025, 3, 9).unwrap(), 17),
            at(2025, 3, 9, 22, 0)
        );
        assert_eq!(
            central_local_to_utc(NaiveDate::from_ymd_opt(2025, 11, 2).unwrap(), 17),
            at(2025, 11, 2, 23, 0)
        );
    }

    #[test]
    fn up_gap_stays_open_through_partial_fill() {
        let close_at = at(2025, 7, 11, 21, 0);
        let reopen_at = at(2025, 7, 13, 22, 0);
        let fill_at = at(2025, 7, 13, 22, 2);
        let source = series(vec![
            point(close_at.saturating_sub(60_000), 99.0, 101.0, 98.0, 100.0),
            point(reopen_at, 110.0, 112.0, 105.0, 108.0),
            point(reopen_at.saturating_add(60_000), 108.0, 109.0, 101.0, 102.0),
            point(fill_at, 102.0, 104.0, 99.0, 101.0),
        ]);
        let gap = build_gap(
            &source,
            SessionWindow {
                close_at,
                reopen_at,
            },
        )
        .unwrap();

        assert_eq!(gap.low, Price::from_f64(100.0));
        assert_eq!(gap.high, Price::from_f64(110.0));
        assert_eq!(gap.filled_at, Some(fill_at));
    }

    #[test]
    fn down_gap_fills_only_at_the_upper_boundary() {
        let close_at = at(2025, 1, 10, 22, 0);
        let reopen_at = at(2025, 1, 12, 23, 0);
        let fill_at = at(2025, 1, 12, 23, 2);
        let source = series(vec![
            point(close_at.saturating_sub(60_000), 111.0, 112.0, 109.0, 110.0),
            point(reopen_at, 100.0, 105.0, 98.0, 103.0),
            point(reopen_at.saturating_add(60_000), 103.0, 109.0, 101.0, 108.0),
            point(fill_at, 108.0, 111.0, 107.0, 110.0),
        ]);
        let gap = build_gap(
            &source,
            SessionWindow {
                close_at,
                reopen_at,
            },
        )
        .unwrap();

        assert_eq!(gap.low, Price::from_f64(100.0));
        assert_eq!(gap.high, Price::from_f64(110.0));
        assert_eq!(gap.filled_at, Some(fill_at));
    }

    #[test]
    fn legacy_weekend_gaps_stop_at_the_24_7_cutover() {
        let windows = legacy_weekend_windows(at(2026, 5, 1, 0, 0), at(2026, 6, 30, 0, 0));

        assert!(
            windows
                .iter()
                .all(|window| window.close_at < legacy_crypto_schedule_cutover())
        );
        assert!(
            windows
                .iter()
                .any(|window| window.close_at == at(2026, 5, 22, 21, 0))
        );
        assert!(
            !windows
                .iter()
                .any(|window| window.close_at == at(2026, 5, 29, 21, 0))
        );
    }
}
