use super::KlineIndicatorImpl;
use super::daily_delta::delta_history_colors;
use super::footprint_history::{
    DAY_MS, FootprintHistoryIndicator, StoredLargeTrade, day_start, draw_text,
};
use crate::chart::{Message, ViewState};

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
/// Above this many visible markers, per-marker labels are suppressed; circle
/// fills alone keep redraws cheap on very low thresholds.
const MAX_LABELED_MARKERS: usize = 300;

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
pub struct LargeTradesIndicator {
    inner: FootprintHistoryIndicator,
    min_usd: f32,
}

impl LargeTradesIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(LARGE_TRADES_LOOKBACK_DAYS);
        Self {
            inner,
            min_usd: KlineChartConfig::LARGE_TRADES_MIN_USD_DEFAULT,
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

        // The day books are keyed by UTC day; walk only the days the window
        // touches and copy just the entries above the threshold.
        let mut candidates: Vec<StoredLargeTrade> = Vec::new();
        let first_day = day_start(UnixMs::new(window.from));
        let last_day = day_start(UnixMs::new(window.to));
        let mut day = first_day;
        loop {
            for trade in self.inner.display_large_trades(day, threshold) {
                let time = trade.time.as_u64();
                if time >= window.from && time <= window.to {
                    candidates.push(trade);
                }
            }
            if day >= last_day {
                break;
            }
            day = day.saturating_add(DAY_MS);
        }

        let mut markers: Vec<(f32, StoredLargeTrade)> = Vec::with_capacity(candidates.len());
        match &window.mapper {
            XMapper::Timestamp => {
                for trade in candidates {
                    markers.push((chart.interval_to_x(trade.time.as_u64()), trade));
                }
            }
            XMapper::Brick {
                first_idx,
                last_idx,
            } => {
                let PlotData::TickBased(tick_aggr) = data_source else {
                    return markers;
                };
                let len = tick_aggr.datapoints.len();
                let (first, last) = (*first_idx, *last_idx);
                for trade in candidates {
                    let closed_bricks = tick_aggr
                        .datapoints
                        .partition_point(|dp| dp.kline.time.as_u64() <= trade.time.as_u64());
                    if closed_bricks == 0 || closed_bricks > len {
                        continue;
                    }
                    let index = (len - closed_bricks) as u64;
                    if !(first..=last).contains(&index) {
                        continue;
                    }
                    markers.push((chart.interval_to_x(index), trade));
                }
            }
        }

        // `region` is the transformed visible region the overlay was drawn
        // with; deriving it from raw widget bounds here would produce a
        // wrong price band and silently cull almost every marker.
        let (highest, lowest) = chart.price_range(&region);
        markers.retain(|(x, trade)| {
            x.is_finite()
                && Price::from_units(trade.price_units) <= highest
                && Price::from_units(trade.price_units) >= lowest
        });
        markers
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
        let max_notional = markers
            .iter()
            .map(|(_, trade)| trade.notional)
            .fold(0.0_f64, f64::max)
            .max(f64::from(self.min_usd.max(capture_floor_usd())));
        let radius_min = (2.25 / scaling).max(1.0);
        let radius_max = (8.5 / scaling).max(radius_min + 0.5);
        let ring_width = (1.1 / scaling).clamp(0.5, 2.0);
        let (buy, sell) = delta_history_colors(palette);
        let label_color = palette.background.base.text.scale_alpha(0.85);
        let show_labels = markers.len() <= MAX_LABELED_MARKERS;

        for (x, trade) in markers {
            let fraction = (trade.notional / max_notional).sqrt();
            let radius = radius_min + (radius_max - radius_min) * fraction as f32;
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

            if show_labels && radius * scaling >= 5.0 {
                let text = format!("${}", abbr_large_numbers(trade.notional));
                draw_text(
                    frame,
                    &text,
                    Point::new(x, y - radius - 6.0 / scaling),
                    (9.0 / scaling).clamp(7.0, 11.0),
                    label_color,
                    Alignment::Center,
                );
            }
        }
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
                trade(base + 1_001, 60_000.0, 2.0, true),   // $120k — kept
            ],
            false,
        );

        let day = day_start(UnixMs::new(base + 1_000));
        assert!(
            indicator
                .inner
                .display_large_trades(day, capture_floor_usd())
                .len()
                == 1
        );
        // The user-facing threshold filters at read time without refetching.
        assert!(
            indicator
                .inner
                .display_large_trades(day, KlineChartConfig::LARGE_TRADES_MIN_USD_DEFAULT)
                .is_empty()
        );
    }

    #[test]
    fn merged_display_spans_all_active_sources() {
        let base = now_base();
        let mut indicator = LargeTradesIndicator::new();
        indicator.configure_footprint_history(&[binance(), bybit()], true);

        indicator.on_source_trades(binance(), &[trade(base + 100, 60_000.0, 1.0, false)], false);
        indicator.on_source_trades(bybit(), &[trade(base + 101, 60_000.0, 1.0, true)], false);

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
            &[trade(cutoff.as_u64() + 1_000, 61_000.0, 2.0, false)],
            false,
        );

        // Today's authoritative backfill is paced last and arrives much
        // later; it covers only up to the cutoff, so the live print must not
        // be dropped or duplicated.
        let day = day_start(UnixMs::new(cutoff.as_u64()));
        indicator.on_source_trades(binance(), &[trade(day + 60_000, 60_000.0, 2.0, true)], true);

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
        indicator.on_source_trades(binance(), &[trade(base + 130, 61_000.0, 1.0, true)], false);

        // Authoritative Binance day batch covering [base+120, base+180].
        indicator.on_source_trades(
            binance(),
            &[
                trade(base + 120, 62_000.0, 2.0, false),
                trade(base + 180, 63_000.0, 2.0, true),
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
        indicator.on_source_trades(binance(), &[trade(base + 100, 60_000.0, 1.0, false)], false);

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
}
