use super::KlineIndicatorImpl;
use super::daily_delta::delta_history_colors;
use super::footprint_history::{
    DAY_MS, DayBookRetain, FootprintHistoryIndicator, StoredLargeTrade, day_start, draw_text,
};
use crate::chart::{Message, ViewState};
use std::cell::RefCell;

use data::chart::PlotData;
use data::chart::kline::{Config as KlineChartConfig, KlineDataPoint};
use data::util::abbr_large_numbers;
use exchange::UnixMs;
use exchange::unit::{Price, PriceStep};
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Alignment, Element, Point, Rectangle};

/// UTC days of multi-venue trade history retained and backfilled for this
/// overlay. The chart's fetch planning uses the same constant so the backfill
/// window matches retention.
pub(crate) const LARGE_TRADES_LOOKBACK_DAYS: u16 = 15;
/// Extra milliseconds padded onto the visible time range so markers do not
/// pop in at the pane edges.
const EDGE_PAD_MS: u64 = 60_000;
/// Hard cap on circles drawn in one frame. Keep the largest prints in view.
const MAX_DRAWN_MARKERS: usize = 400;
/// Same-side prints whose timestamps fall in this window are drawn as one
/// bubble (notional summed, VWAP price). Matches Binance aggTrades' 100ms
/// compression, and also folds simultaneous multi-venue prints together.
const CLUSTER_WINDOW_MS: u64 = 100;

fn cluster_same_side_prints(mut prints: Vec<StoredLargeTrade>) -> Vec<StoredLargeTrade> {
    if prints.len() <= 1 {
        return prints;
    }
    prints.sort_by_key(|trade| (trade.is_sell, trade.time.as_u64()));
    let mut clustered = Vec::with_capacity(prints.len());
    let mut start = 0;
    while start < prints.len() {
        let side = prints[start].is_sell;
        let window_start = prints[start].time.as_u64();
        let mut end = start + 1;
        while end < prints.len()
            && prints[end].is_sell == side
            && prints[end].time.as_u64().saturating_sub(window_start) <= CLUSTER_WINDOW_MS
        {
            end += 1;
        }
        clustered.push(merge_print_cluster(&prints[start..end]));
        start = end;
    }
    clustered
}

fn merge_print_cluster(prints: &[StoredLargeTrade]) -> StoredLargeTrade {
    if prints.len() == 1 {
        return prints[0];
    }
    let mut notional = 0.0_f64;
    let mut qty = 0.0_f64;
    let mut time_weighted = 0.0_f64;
    for print in prints {
        let price = Price::from_units(print.price_units).to_f64();
        let size = if price > 0.0 {
            print.notional / price
        } else {
            0.0
        };
        notional += print.notional;
        qty += size;
        time_weighted += print.time.as_u64() as f64 * print.notional;
    }
    let vwap = if qty > 0.0 {
        notional / qty
    } else {
        Price::from_units(prints[0].price_units).to_f64()
    };
    let time = if notional > 0.0 {
        (time_weighted / notional).round() as u64
    } else {
        prints[0].time.as_u64()
    };
    StoredLargeTrade {
        time: UnixMs::new(time),
        price_units: Price::from_f64(vwap).units,
        notional,
        is_sell: prints[0].is_sell,
    }
}

fn marker_radius(notional: f64, min_notional: f64, max_notional: f64, scaling: f32) -> f32 {
    let min_n = min_notional.max(1.0);
    let max_n = max_notional.max(min_n * 2.0);
    let span = (max_n / min_n).ln().max(f64::EPSILON);
    let t = ((notional / min_n).max(1.0).ln() / span).clamp(0.0, 1.0) as f32;
    // Steeper than linear at the top so 10M vs 40M reads as a size jump.
    let fraction = t.powf(1.35);
    let radius_min = (4.0 / scaling).max(2.0);
    let radius_max = (32.0 / scaling).max(radius_min + 10.0);
    radius_min + (radius_max - radius_min) * fraction
}

fn capture_floor_usd() -> f32 {
    KlineChartConfig::LARGE_TRADES_MIN_USD_MIN
}

/// How a trade timestamp maps to an x position for the current chart basis.
enum XMapper {
    /// Time-based charts map timestamps directly.
    Timestamp,
    /// Tick/Renko/TPO bricks map through the brick index that was open at the
    /// trade time. `first_idx`/`last_idx` bound the visible indices (larger is
    /// older).
    Brick { first_idx: u64, last_idx: u64 },
}

struct VisibleWindow {
    from: u64,
    to: u64,
    mapper: XMapper,
}

/// Plots unusually large executed trades as circles on the main candle canvas.
///
/// Storage is delegated to the shared multi-venue day book
/// ([`FootprintHistoryIndicator`]), exactly like Daily Delta: per-venue UTC-day
/// books with disk persistence, live/backfill seam buffering, retention pruning
/// and precise invalidation all come from that runtime. Trades at or above the
/// fixed capture floor are recorded by `DayStats::insert_trade`; the
/// user-facing threshold filters at draw time, so slider changes are instant
/// and lossless within the retention window.
///
/// Drawing copies only the entries above the threshold for the visible days,
/// so a redraw costs O(visible markers), not O(day book).
struct HoverMarker {
    x: f32,
    y: f32,
    radius: f32,
    notional: f64,
}

pub struct LargeTradesIndicator {
    inner: FootprintHistoryIndicator,
    min_usd: f32,
    /// Filled by `draw_overlay` and reused by hover so mouse-move crosshair
    /// frames do not re-scan the day books.
    hover_markers: RefCell<Vec<HoverMarker>>,
}

impl LargeTradesIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(LARGE_TRADES_LOOKBACK_DAYS);
        inner.set_day_book_retain(DayBookRetain::LARGE_TRADES_ONLY);
        Self {
            inner,
            min_usd: KlineChartConfig::LARGE_TRADES_MIN_USD_DEFAULT,
            hover_markers: RefCell::new(Vec::new()),
        }
    }

    pub fn set_threshold(&mut self, min_usd: f32) {
        self.min_usd = min_usd.clamp(
            KlineChartConfig::LARGE_TRADES_MIN_USD_MIN,
            KlineChartConfig::LARGE_TRADES_MIN_USD_MAX,
        );
    }

    fn visible_window(
        &self,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        region: Rectangle,
    ) -> Option<VisibleWindow> {
        let (earliest, latest) = chart.interval_range(&region);
        match data_source {
            PlotData::TimeBased(_) => Some(VisibleWindow {
                from: earliest.saturating_sub(EDGE_PAD_MS),
                to: latest.saturating_add(EDGE_PAD_MS),
                mapper: XMapper::Timestamp,
            }),
            PlotData::TickBased(tick_aggr) => {
                // Datapoints are stored oldest-first; a datapoint at vector
                // position `p` has ViewState x-axis index `len - 1 - p`, so
                // larger indices are older. Bound the scan by the times of the
                // visible boundary bricks.
                let len = tick_aggr.datapoints.len();
                if len == 0 {
                    return None;
                }
                let first_idx = earliest.min(latest);
                let last_idx = earliest.max(latest);

                let oldest_time = if last_idx < len as u64 {
                    tick_aggr.datapoints[len - 1 - last_idx as usize]
                        .kline
                        .time
                        .as_u64()
                } else {
                    u64::MIN
                };
                let newest_time = if first_idx < len as u64 {
                    tick_aggr.datapoints[len - 1 - first_idx as usize]
                        .kline
                        .time
                        .as_u64()
                } else {
                    tick_aggr.datapoints[len - 1].kline.time.as_u64()
                };

                Some(VisibleWindow {
                    from: oldest_time.saturating_sub(EDGE_PAD_MS),
                    to: newest_time.saturating_add(EDGE_PAD_MS),
                    mapper: XMapper::Brick {
                        first_idx,
                        last_idx,
                    },
                })
            }
        }
    }

    fn collect_markers(
        &self,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        window: &VisibleWindow,
        region: Rectangle,
    ) -> Vec<(f32, StoredLargeTrade)> {
        let threshold = self.min_usd.max(capture_floor_usd());
        // `region` is the transformed visible region the overlay was drawn
        // with; deriving it from raw widget bounds here would produce a
        // wrong price band and silently cull almost every marker.
        let (highest, lowest) = chart.price_range(&region);

        let mut prints: Vec<StoredLargeTrade> = Vec::new();
        let first_day = day_start(UnixMs::new(window.from));
        let last_day = day_start(UnixMs::new(window.to));
        let mut day = first_day;
        loop {
            self.inner.for_each_large_trade(day, threshold, |trade| {
                let time = trade.time.as_u64();
                if time < window.from || time > window.to {
                    return;
                }
                let price = Price::from_units(trade.price_units);
                if price > highest || price < lowest {
                    return;
                }
                prints.push(trade);
            });
            if day >= last_day {
                break;
            }
            day = day.saturating_add(DAY_MS);
        }

        let clustered = cluster_same_side_prints(prints);
        let mut markers: Vec<(f32, StoredLargeTrade)> = Vec::with_capacity(clustered.len());
        for trade in clustered {
            let Some(x) = self.marker_x(chart, data_source, window, trade.time.as_u64()) else {
                continue;
            };
            if !x.is_finite() {
                continue;
            }
            markers.push((x, trade));
        }

        if markers.len() > MAX_DRAWN_MARKERS {
            markers.select_nth_unstable_by(MAX_DRAWN_MARKERS, |left, right| {
                right.1.notional.total_cmp(&left.1.notional)
            });
            markers.truncate(MAX_DRAWN_MARKERS);
        }
        markers
    }

    fn marker_x(
        &self,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        window: &VisibleWindow,
        time: u64,
    ) -> Option<f32> {
        match &window.mapper {
            XMapper::Timestamp => Some(chart.interval_to_x(time)),
            XMapper::Brick {
                first_idx,
                last_idx,
            } => {
                let PlotData::TickBased(tick_aggr) = data_source else {
                    return None;
                };
                let len = tick_aggr.datapoints.len();
                let closed_bricks = tick_aggr
                    .datapoints
                    .partition_point(|dp| dp.kline.time.as_u64() <= time);
                if closed_bricks == 0 || closed_bricks > len {
                    return None;
                }
                let index = (len - closed_bricks) as u64;
                (*first_idx..=*last_idx)
                    .contains(&index)
                    .then(|| chart.interval_to_x(index))
            }
        }
    }
}

impl Default for LargeTradesIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl KlineIndicatorImpl for LargeTradesIndicator {
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
        self.inner.set_lookback_days(LARGE_TRADES_LOOKBACK_DAYS);
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

    fn reset_trade_history_backfill(&mut self) {
        self.inner.reset_trade_history_backfill();
    }

    fn on_source_trades(
        &mut self,
        source: exchange::TickerInfo,
        trades: &[exchange::Trade],
        historical: bool,
    ) {
        // Seam handling (live-tail protection across day rebuilds), retention
        // and persistence are owned by the shared runtime.
        self.inner.on_source_trades(source, trades, historical);
    }

    fn set_large_trades_threshold(&mut self, min_notional_usd: f32) {
        self.set_threshold(min_notional_usd);
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
        let Some(window) = self.visible_window(chart, data_source, region) else {
            return;
        };
        let markers = self.collect_markers(chart, data_source, &window, region);
        if markers.is_empty() {
            return;
        }

        let scaling = chart.scaling.max(0.01);
        let min_notional = f64::from(self.min_usd.max(capture_floor_usd()));
        let max_notional = markers
            .iter()
            .map(|(_, trade)| trade.notional)
            .fold(min_notional, f64::max);
        let ring_width = (1.2 / scaling).clamp(0.6, 2.4);
        let (buy, sell) = delta_history_colors(palette);

        let mut hover = Vec::with_capacity(markers.len());
        for (x, trade) in markers {
            let radius = marker_radius(trade.notional, min_notional, max_notional, scaling);
            if !radius.is_finite() || radius <= 0.0 {
                continue;
            }
            let y = chart.price_to_y(Price::from_units(trade.price_units));
            if !y.is_finite() {
                continue;
            }
            let center = Point::new(x, y);
            let base = if trade.is_sell { sell } else { buy };
            let circle = Path::circle(center, radius);

            frame.fill(&circle, base.scale_alpha(0.45));
            frame.stroke(
                &circle,
                Stroke::with_color(
                    Stroke {
                        width: ring_width,
                        ..Stroke::default()
                    },
                    base.scale_alpha(0.95),
                ),
            );
            hover.push(HoverMarker {
                x,
                y,
                radius,
                notional: trade.notional,
            });
        }
        *self.hover_markers.borrow_mut() = hover;
    }

    fn draw_hover(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        _data_source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        _region: Rectangle,
        cursor: Point,
    ) {
        let markers = self.hover_markers.borrow();
        if markers.is_empty() {
            return;
        }

        let mut hovered: Option<(f32, Point, f32, f64)> = None;
        for marker in markers.iter() {
            let dx = cursor.x - marker.x;
            let dy = cursor.y - marker.y;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq > marker.radius * marker.radius {
                continue;
            }
            let replace = hovered.is_none_or(|(best_dist, _, _, _)| dist_sq < best_dist);
            if replace {
                hovered = Some((
                    dist_sq,
                    Point::new(marker.x, marker.y),
                    marker.radius,
                    marker.notional,
                ));
            }
        }

        let Some((_, center, radius, notional)) = hovered else {
            return;
        };
        let scaling = chart.scaling.max(0.01);
        let label_color = palette.background.base.text;
        let text = format!("${}", abbr_large_numbers(notional));
        draw_text(
            frame,
            &text,
            Point::new(center.x, center.y - radius - 8.0 / scaling),
            (10.0 / scaling).clamp(8.0, 13.0),
            label_color,
            Alignment::Center,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;
    use exchange::unit::Qty;
    use exchange::{Ticker, TickerInfo, Trade};

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    fn ticker_info(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    fn binance() -> TickerInfo {
        ticker_info(Exchange::BinanceLinear, "BTCUSDT")
    }

    fn bybit() -> TickerInfo {
        ticker_info(Exchange::BybitLinear, "BTCUSDT")
    }

    /// Retention pruning drops anything older than the lookback window, so
    /// tests anchor timestamps to now.
    fn now_base() -> u64 {
        UnixMs::now().as_u64()
    }

    #[test]
    fn captures_above_floor_into_shared_day_books() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);

        indicator.on_source_trades(
            binance(),
            &[
                trade(base + 1_000, 60_000.0, 0.05, false), // $3k — dropped
                trade(base + 1_001, 60_000.0, 25.0, true),  // $1.5M — kept
            ],
            false,
        );

        let day = day_start(UnixMs::new(base + 1_000));
        assert_eq!(
            indicator
                .inner
                .display_large_trades(day, capture_floor_usd())
                .len(),
            1
        );
        // The user-facing threshold filters at read time without refetching.
        assert!(
            indicator
                .inner
                .display_large_trades(day, 5_000_000.0)
                .is_empty()
        );
    }

    #[test]
    fn merged_display_spans_all_active_sources() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance(), bybit()], true);

        indicator.on_source_trades(
            binance(),
            &[trade(base + 100, 60_000.0, 20.0, false)],
            false,
        );
        indicator.on_source_trades(bybit(), &[trade(base + 101, 60_000.0, 20.0, true)], false);

        let day = day_start(UnixMs::new(base + 100));
        let merged = indicator
            .inner
            .display_large_trades(day, capture_floor_usd());
        assert_eq!(merged.len(), 2);
        assert!(merged[0].time.as_u64() <= merged[1].time.as_u64());
    }

    #[test]
    fn rally_prints_captured_live_survive_the_late_day_backfill() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);
        let cutoff = UnixMs::new(base.saturating_sub(3_600_000));
        indicator.prepare_footprint_history(binance(), cutoff);

        // A large print lands live, after the session started.
        indicator.on_source_trades(
            binance(),
            &[trade(cutoff.as_u64() + 1_000, 61_000.0, 20.0, false)],
            false,
        );

        // Today's authoritative backfill is paced last and arrives much
        // later; it covers only up to the cutoff, so the live print must not
        // be dropped or duplicated.
        let day = day_start(UnixMs::new(cutoff.as_u64()));
        indicator.on_source_trades(
            binance(),
            &[trade(day + 60_000, 60_000.0, 20.0, true)],
            true,
        );

        let merged = indicator
            .inner
            .display_large_trades(day, capture_floor_usd());
        assert_eq!(merged.len(), 2);
        assert!(
            merged
                .iter()
                .any(|t| t.price_units == Price::from_f64(61_000.0).units)
        );
    }

    #[test]
    fn historical_batches_do_not_duplicate_live_captures() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);

        // Live tape inside the upcoming backfill window.
        indicator.on_source_trades(binance(), &[trade(base + 130, 61_000.0, 20.0, true)], false);

        // Authoritative Binance day batch covering [base+120, base+180].
        indicator.on_source_trades(
            binance(),
            &[
                trade(base + 120, 62_000.0, 20.0, false),
                trade(base + 180, 63_000.0, 20.0, true),
            ],
            true,
        );

        let day = day_start(UnixMs::new(base + 130));
        let trades = indicator
            .inner
            .display_large_trades(day, capture_floor_usd());
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].price_units, Price::from_f64(62_000.0).units);
        assert_eq!(trades[1].price_units, Price::from_f64(63_000.0).units);
    }

    #[test]
    fn resetting_backfill_clears_the_shared_books() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);
        indicator.on_source_trades(
            binance(),
            &[trade(base + 100, 60_000.0, 20.0, false)],
            false,
        );

        indicator.reset_trade_history_backfill();

        let day = day_start(UnixMs::new(base + 100));
        assert!(
            indicator
                .inner
                .display_large_trades(day, capture_floor_usd())
                .is_empty()
        );
    }

    #[test]
    fn million_dollar_print_survives_the_user_threshold() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);
        indicator.set_threshold(1_090_000.0);

        indicator.on_source_trades(
            binance(),
            &[
                trade(base + 1, 80_000.0, 0.2, false), // $16k — below the slider
                trade(base + 2, 80_000.0, 20.0, false), // $1.6M — the rally print
            ],
            false,
        );

        let day = day_start(UnixMs::new(base + 1));
        let shown = indicator.inner.display_large_trades(day, 1_090_000.0);
        assert_eq!(shown.len(), 1);
        assert!((shown[0].notional - 1_600_000.0).abs() < 1.0);
    }

    fn stored(time: u64, price: f64, notional: f64, is_sell: bool) -> StoredLargeTrade {
        StoredLargeTrade {
            time: UnixMs::new(time),
            price_units: Price::from_f64(price).units,
            notional,
            is_sell,
        }
    }

    #[test]
    fn same_side_prints_inside_100ms_merge_into_one_bubble() {
        let clustered = cluster_same_side_prints(vec![
            stored(1_000, 80_000.0, 1_200_000.0, false),
            stored(1_040, 80_200.0, 1_800_000.0, false),
            stored(1_090, 80_100.0, 1_000_000.0, false),
        ]);
        assert_eq!(clustered.len(), 1);
        assert!(!clustered[0].is_sell);
        assert!((clustered[0].notional - 4_000_000.0).abs() < 1.0);
        let vwap = Price::from_units(clustered[0].price_units).to_f64();
        assert!((vwap - 80_110.0).abs() < 50.0);
    }

    #[test]
    fn opposite_sides_in_the_same_window_stay_two_bubbles() {
        let clustered = cluster_same_side_prints(vec![
            stored(1_000, 80_000.0, 1_200_000.0, false),
            stored(1_010, 80_000.0, 1_200_000.0, true),
        ]);
        assert_eq!(clustered.len(), 2);
    }

    #[test]
    fn prints_farther_than_100ms_are_not_clustered() {
        let clustered = cluster_same_side_prints(vec![
            stored(1_000, 80_000.0, 1_200_000.0, true),
            stored(1_150, 80_000.0, 1_200_000.0, true),
        ]);
        assert_eq!(clustered.len(), 2);
    }

    #[test]
    fn threshold_clamps_to_configured_range() {
        let mut indicator = LargeTradesIndicator::new();
        indicator.set_threshold(1.0);
        assert_eq!(
            indicator.min_usd,
            KlineChartConfig::LARGE_TRADES_MIN_USD_MIN
        );
        indicator.set_threshold(99_999_999.0);
        assert_eq!(
            indicator.min_usd,
            KlineChartConfig::LARGE_TRADES_MIN_USD_MAX
        );
    }

    #[test]
    fn overlay_ingest_does_not_build_price_level_maps() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance()], true);
        indicator.on_source_trades(
            binance(),
            &[
                trade(base + 1_000, 60_000.0, 0.05, false),
                trade(base + 1_001, 60_000.0, 25.0, true),
            ],
            false,
        );

        let day = day_start(UnixMs::new(base + 1_000));
        let shown = indicator.inner.display_day_at(day);
        assert!(shown.stats.levels.is_empty());
        assert!(shown.stats.five_min_deltas().is_empty());
        assert_eq!(
            indicator
                .inner
                .display_large_trades(day, capture_floor_usd())
                .len(),
            1
        );
    }
}
