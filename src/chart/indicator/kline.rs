use crate::chart::{Basis, Message, ViewState};
use crate::connector::fetcher::FetchRange;

use data::chart::indicator::KlineIndicator;
use data::chart::kline::KlineDataPoint;
use data::chart::{BasisSeries, PlotData};
use exchange::adapter::Exchange;
use exchange::unit::PriceStep;
use exchange::{Kline, Timeframe, Trade, UnixMs};
use iced::theme::palette::Extended;
use iced::{Rectangle, widget::canvas};

use super::plot::AnySeries;

pub mod bar_analysis;
pub mod cme_gap;
pub mod cumulative_delta;
pub mod daily_delta;
pub mod footprint_history;
pub mod large_trades;
pub mod liquidity_heatmap;
pub mod open_interest;
pub mod previous_value_area;
pub mod volume;
pub mod vpvr;

/// UI adapter methods for converting domain `BasisSeries` into plot-ready series.
trait BasisSeriesExt<T> {
    fn as_plot_series(&self) -> AnySeries<'_, T>;
}

impl<T> BasisSeriesExt<T> for BasisSeries<T> {
    fn as_plot_series(&self) -> AnySeries<'_, T> {
        match self {
            BasisSeries::Time(data) => AnySeries::forward_unix_ms(data),
            BasisSeries::Tick(data) => AnySeries::reversed_u64(data),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq)]
pub enum IndicatorAvailability {
    /// Indicator can be rendered normally.
    #[default]
    Available,
    /// Availability cannot be determined yet (e.g. no datapoints loaded).
    Unknown,
    /// Indicator cannot be rendered for the current source/context.
    Unavailable(AvailabilityCause),
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum AvailabilityCause {
    Exchange(Exchange),
    Timeframe(Timeframe),
    Basis(Basis),
    TradeData,
}

impl IndicatorAvailability {
    pub fn unavailable_message(&self, indicator: &str) -> Option<String> {
        match self {
            IndicatorAvailability::Available | IndicatorAvailability::Unknown => None,
            IndicatorAvailability::Unavailable(cause) => Some(match cause {
                AvailabilityCause::Exchange(exchange) => {
                    format!("{indicator} is not available for {exchange}.")
                }
                AvailabilityCause::Timeframe(timeframe) => {
                    format!("{indicator} is not available on {timeframe} timeframe.")
                }
                AvailabilityCause::Basis(Basis::Tick(_)) => {
                    format!("{indicator} is not available for tick charts.")
                }
                AvailabilityCause::Basis(basis) => {
                    format!("{indicator} is not available on {basis} basis.")
                }
                AvailabilityCause::TradeData => {
                    format!("{indicator} requires directional trade-volume data.")
                }
            }),
        }
    }
}

pub trait KlineIndicatorImpl {
    /// Clear all caches for a full redraw
    fn clear_all_caches(&mut self);

    /// Clear caches related to crosshair only
    /// e.g. tooltips and scale labels for a partial redraw
    fn clear_crosshair_caches(&mut self);

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        // Whether to show last value labels on top right/left when not hovering
        data_labels_always_visible: bool,
        visible_range: std::ops::RangeInclusive<u64>,
    ) -> iced::Element<'a, Message>;

    fn availability(&self, _chart: &ViewState) -> IndicatorAvailability {
        IndicatorAvailability::Available
    }

    fn unavailable_message(&self, chart: &ViewState, indicator: &str) -> Option<String> {
        self.availability(chart).unavailable_message(indicator)
    }

    /// If the indicator needs data fetching, return the required range
    fn fetch_range(&mut self, _ctx: &FetchCtx) -> Option<FetchRange> {
        None
    }

    /// Rebuild data using kline(OHLCV) source
    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn on_insert_klines(&mut self, _klines: &[Kline], _source: &PlotData<KlineDataPoint>) {}

    fn on_insert_trades(
        &mut self,
        _trades: &[Trade],
        _old_dp_len: usize,
        _source: &PlotData<KlineDataPoint>,
    ) {
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    /// Timeframe/tick interval has changed
    fn on_basis_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    /// Configure venue sources for the aggregated open-interest indicator.
    fn configure_open_interest(&mut self, _sources: &[exchange::TickerInfo]) {}

    fn open_interest_sources(&self) -> &[exchange::TickerInfo] {
        &[]
    }

    /// Configure the independent Footprint History venue set.
    fn configure_footprint_history(&mut self, _sources: &[exchange::TickerInfo], _aggregate: bool) {
    }

    /// Minimum executed-trade notional shown by the Large Trades overlay.
    fn set_large_trades_threshold(&mut self, _min_notional_usd: f32) {}

    /// Set the historical/live boundary before a source backfill begins.
    fn prepare_footprint_history(&mut self, _source: exchange::TickerInfo, _cutoff: UnixMs) {}

    /// Load one completed UTC-day trade book from the shared disk cache.
    fn load_cached_footprint_day(
        &mut self,
        _source: exchange::TickerInfo,
        _day_start: UnixMs,
    ) -> Option<UnixMs> {
        None
    }

    /// Persist one completed UTC-day trade book to the shared disk cache.
    fn persist_cached_footprint_day(
        &mut self,
        _source: exchange::TickerInfo,
        _day_start: UnixMs,
        _covered_through: UnixMs,
    ) {
    }

    /// Source-aware trades used only by Footprint History.
    fn on_source_trades(
        &mut self,
        _source: exchange::TickerInfo,
        _trades: &[Trade],
        _historical: bool,
    ) {
    }

    /// Accumulate a streamed historical request without exposing a partial
    /// prefix to the canonical day books. Implementations commit the staged
    /// delta only after the fetcher's terminal completion signal.
    fn stage_source_trades(
        &mut self,
        _req_id: uuid::Uuid,
        source: exchange::TickerInfo,
        trades: &[Trade],
    ) {
        self.on_source_trades(source, trades, true);
    }

    /// Atomically publish every staged page owned by `req_id`.
    fn commit_staged_source_trades(&mut self, _req_id: uuid::Uuid) {}

    /// Record exact uncovered intervals after publishing only recorder-proven
    /// pages. Implementations must keep these ranges out of durable completion
    /// checkpoints and any calculations that require continuous history.
    fn mark_incomplete_trade_history(
        &mut self,
        _source: exchange::TickerInfo,
        _missing_ranges: &[(UnixMs, UnixMs)],
    ) {
    }

    /// Discard every staged page owned by a failed, cancelled or stale fetch.
    fn discard_staged_source_trades(&mut self, _req_id: uuid::Uuid) {}

    /// Source-aware open interest used only by Footprint History.
    fn on_source_open_interest(
        &mut self,
        _source: exchange::TickerInfo,
        _values: &[exchange::OpenInterest],
    ) {
    }

    /// How many UTC days of trade history this indicator should retain.
    fn set_trade_history_lookback(&mut self, _days: u16) {}

    /// Sync Previous Value Areas with the chart's letter-timeframe bar store.
    ///
    /// `bars` is `Some` when the store changed since the last sync; `None`
    /// only re-evaluates config changes and period rollovers.
    fn sync_value_areas(
        &mut self,
        _bars: Option<&[Kline]>,
        _config: data::chart::tpo::Config,
        _row_step: PriceStep,
        _now: UnixMs,
    ) -> bool {
        false
    }

    /// Drop cached UTC-day books so the next backfill is not merged on top.
    fn reset_trade_history_backfill(&mut self) {}

    /// Overlay drawn on the main chart canvas. Default is a no-op.
    fn draw_overlay(
        &self,
        _frame: &mut canvas::Frame,
        _chart: &ViewState,
        _data_source: &PlotData<KlineDataPoint>,
        _palette: &Extended,
        _region: Rectangle,
        _group_step: PriceStep,
    ) {
    }

    /// Overlay hover chrome drawn on the crosshair layer in chart space.
    fn draw_hover(
        &self,
        _frame: &mut canvas::Frame,
        _chart: &ViewState,
        _data_source: &PlotData<KlineDataPoint>,
        _palette: &Extended,
        _region: Rectangle,
        _cursor: iced::Point,
    ) {
    }
}

pub struct FetchCtx<'a> {
    pub main_chart: &'a ViewState,
    pub timeframe: Timeframe,
    pub visible_earliest: UnixMs,
    pub kline_latest: UnixMs,
    pub prefetch_earliest: UnixMs,
}

pub fn make_empty(which: KlineIndicator) -> Box<dyn KlineIndicatorImpl> {
    match which {
        KlineIndicator::Volume => Box::new(super::kline::volume::VolumeIndicator::new()),
        KlineIndicator::BarAnalysis => {
            Box::new(super::kline::bar_analysis::BarAnalysisIndicator::new())
        }
        KlineIndicator::CumulativeDelta => {
            Box::new(super::kline::cumulative_delta::CumulativeDeltaIndicator::new())
        }
        KlineIndicator::OpenInterest => {
            Box::new(super::kline::open_interest::OpenInterestIndicator::new())
        }
        KlineIndicator::FootprintHistory => {
            Box::new(super::kline::footprint_history::FootprintHistoryIndicator::new_display())
        }
        KlineIndicator::DailyDelta => {
            Box::new(super::kline::daily_delta::DailyDeltaIndicator::new())
        }
        KlineIndicator::CmeGap => Box::new(super::kline::cme_gap::CmeGapIndicator::new()),
        KlineIndicator::PreviousValueArea => {
            Box::new(super::kline::previous_value_area::PreviousValueAreaIndicator::new())
        }
        KlineIndicator::LiquidityHeatmap => {
            Box::new(super::kline::liquidity_heatmap::LiquidityHeatmapIndicator)
        }
        KlineIndicator::LargeTrades => {
            Box::new(super::kline::large_trades::LargeTradesIndicator::new())
        }
        KlineIndicator::VisibleRangeProfile => {
            Box::new(super::kline::vpvr::VisibleRangeProfileIndicator::new())
        }
    }
}
