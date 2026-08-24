use super::{
    Action, Basis, Chart, Interaction, Message, PlotConstants, PlotData, TEXT_SIZE, ViewState,
    indicator, request_fetch, request_fetch_with_stream, scale::linear::PriceInfoLabel,
};
use crate::chart::indicator::kline::KlineIndicatorImpl;
use crate::chart::indicator::kline::daily_delta::delta_history_colors;
use crate::chart::indicator::kline::footprint_history::{
    DAYS as FOOTPRINT_HISTORY_DAYS, day_start, missing_day_range, utc_day_ranges,
};
use crate::chart::indicator::kline::large_trades::LARGE_TRADES_LOOKBACK_DAYS;
use crate::chart::indicator::kline::previous_value_area::value_area_history_earliest;
use crate::connector::fetcher::{
    FetchRange, FetchSpec, ReqError, RequestHandler, TradeFetchMode, is_trade_fetch_enabled,
    trade_fetch_mode,
};
use crate::widget::chart::heatmap::HeatmapShader;
use crate::{modal::pane::settings::study, style};
use data::aggr::ticks::TickAggr;
use data::aggr::time::TimeSeries;
use data::aggregation::{DepthAggregator, KlineAggregator, ResolvedFeed};
use data::chart::indicator::{Indicator, KlineIndicator};
use data::chart::kline::{
    ClusterKind, ClusterScaling, Config, FootprintStudy, KlineDataPoint, KlineTrades, NPoc,
    PointOfControl, RenkoConfig,
};
use data::chart::tpo::{
    Config as TpoConfig, DisplayStyle as TpoDisplayStyle, Profile as TpoProfile,
    ProfilePeriod as TpoProfilePeriod, StructuralComposite, active_three_day_composite,
    block_letter, price_to_row,
};
use data::chart::{Autoscale, KlineChartKind, ViewConfig};

use data::config::theme::{composite_color, contrast_ratio, mix_color};
use data::util::abbr_large_numbers;
use exchange::adapter::{StreamKind, Venue};
use exchange::depth::Depth;
use exchange::unit::{Price, PriceStep};
use exchange::{
    Kline, OpenInterest as OIData, TickMultiplier, TickerInfo, Timeframe, Trade, UnixMs,
};

use iced::task::Handle;
use iced::theme::palette::Extended;
use iced::widget::canvas::{self, Event, Geometry, Path, Stroke};
use iced::{Alignment, Color, Element, Point, Rectangle, Renderer, Size, Theme, Vector, mouse};

use enum_map::EnumMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::time::{Duration, Instant};

const RENKO_SEED_TIMEFRAME: exchange::Timeframe = exchange::Timeframe::M1;
/// Publish losslessly-ingested live data to the expensive canvas at 20 Hz.
/// Interaction-driven invalidation remains immediate, so this reduces render
/// contention without making pan/zoom input wait behind market-data redraws.
pub(crate) const LIVE_REDRAW_INTERVAL_MS: u64 = 50;
const LIVE_REDRAW_INTERVAL: Duration = Duration::from_millis(LIVE_REDRAW_INTERVAL_MS);
/// A month of close-based seed bars. Each REST page is one venue-sized
/// window of 1m closes, requested one page per seed tick so a liquid
/// symbol cannot stall the UI with a giant rebuild.
const RENKO_HISTORY_MS: u64 = 31 * 24 * 60 * 60 * 1_000;

fn renko_seed_page_bars(source: TickerInfo) -> u64 {
    match source.exchange().venue() {
        Venue::Hyperliquid => 5_000,
        Venue::Okex => 300,
        _ => 1_000,
    }
}

fn tpo_history_earliest(config: TpoConfig, now: UnixMs) -> UnixMs {
    config.profile_start(now.saturating_sub(config.history_range_ms()))
}

fn shared_history_price_step(sources: &[TickerInfo], fallback: PriceStep) -> PriceStep {
    sources
        .first()
        .and_then(|source| data::aggregation::AggregateFeedId::for_seed_ticker(source.ticker))
        .map(data::aggregation::AggregateFeedId::price_step)
        .unwrap_or_else(|| {
            sources
                .iter()
                .map(|source| PriceStep::from(source.min_ticksize))
                .min_by_key(|step| step.units)
                .unwrap_or(fallback)
        })
}

fn previous_value_area_price_step(
    sources: &[TickerInfo],
    fallback: PriceStep,
    ticks: u16,
) -> PriceStep {
    let min_tick = sources
        .iter()
        .map(|source| PriceStep::from(source.min_ticksize))
        .min_by_key(|step| step.units)
        .unwrap_or(fallback);
    TickMultiplier(ticks.max(1)).multiply_step(min_tick)
}

fn trade_history_available(source: TickerInfo, fetch_mode: TradeFetchMode) -> bool {
    match fetch_mode {
        TradeFetchMode::Off => false,
        TradeFetchMode::Exchange => {
            matches!(source.exchange().venue(), Venue::Binance | Venue::Bybit)
        }
        TradeFetchMode::Server => true,
    }
}

type DayRange = (UnixMs, UnixMs);

fn footprint_history_day_ranges(cutoff: UnixMs, recent_lookback_days: u64) -> Vec<DayRange> {
    if recent_lookback_days == 0 {
        return Vec::new();
    }
    utc_day_ranges(cutoff, recent_lookback_days)
        .into_iter()
        .rev()
        .collect()
}

struct FootprintHistoryRuntime {
    sources: Vec<TickerInfo>,
    aggregate: bool,
    cutoff: Option<UnixMs>,
    trade_requests: FxHashMap<uuid::Uuid, FootprintTradeRequest>,
    oi_requests: FxHashSet<uuid::Uuid>,
    fetch_handles: Vec<Handle>,
    liquidity_sources: Vec<TickerInfo>,
    liquidity: Option<Box<LiquidityHeatmapRuntime>>,
    last_overlay_redraw: Option<Instant>,
}

struct LiquidityHeatmapRuntime {
    heatmap: Box<HeatmapShader>,
    depth: Box<DepthAggregator>,
}

#[derive(Clone, Copy)]
struct FootprintTradeRequest {
    source: TickerInfo,
    day_start: UnixMs,
    covered_through: UnixMs,
}

impl FootprintHistoryRuntime {
    fn new(source: TickerInfo) -> Self {
        Self {
            sources: vec![source],
            aggregate: true,
            cutoff: None,
            trade_requests: FxHashMap::default(),
            oi_requests: FxHashSet::default(),
            fetch_handles: Vec::new(),
            liquidity_sources: vec![source],
            liquidity: None,
            last_overlay_redraw: None,
        }
    }
}

impl Chart for KlineChart {
    type IndicatorKind = KlineIndicator;

    fn state(&self) -> &ViewState {
        &self.chart
    }

    fn mut_state(&mut self) -> &mut ViewState {
        &mut self.chart
    }

    fn invalidate_crosshair(&mut self) {
        self.chart.cache.clear_crosshair();
        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indi| indi.clear_crosshair_caches());
    }

    fn invalidate_all(&mut self) {
        self.invalidate(None);
    }

    fn view_indicators(&'_ self, enabled: &[Self::IndicatorKind]) -> Vec<Element<'_, Message>> {
        let chart_state = self.state();
        let visible_region = chart_state.visible_region(chart_state.bounds.size());
        let (earliest, latest) = chart_state.interval_range(&visible_region);
        if earliest > latest {
            return vec![];
        }

        let data_labels_always_visible = self.visual_config.data_labels_always_visible;

        let market = chart_state.ticker_info.market_type();
        let mut elements = vec![];

        for selected_indicator in enabled {
            if selected_indicator.is_overlay()
                || !self.kind.allows_indicator(*selected_indicator)
                || !KlineIndicator::for_market(market).contains(selected_indicator)
            {
                continue;
            }
            if let Some(indi) = self.indicators[*selected_indicator].as_ref() {
                elements.push(indi.element(
                    chart_state,
                    data_labels_always_visible,
                    earliest..=latest,
                ));
            }
        }
        elements
    }

    fn visible_timerange(&self) -> Option<(u64, u64)> {
        let chart = self.state();
        let region = chart.visible_region(chart.bounds.size());

        if region.width == 0.0 {
            return None;
        }

        Some(chart.interval_range(&region))
    }

    fn interval_keys(&self) -> Option<&[data::aggr::ticks::TickAccumulation]> {
        match &self.data_source {
            PlotData::TimeBased(_) => None,
            PlotData::TickBased(tick_aggr) => Some(&tick_aggr.datapoints),
        }
    }

    fn autoscaled_coords(&self) -> Vector {
        let chart = self.state();
        let x_translation = match &self.kind {
            KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. } => {
                0.5 * (chart.bounds.width / chart.scaling) - (chart.cell_width / chart.scaling)
            }
            KlineChartKind::Candles | KlineChartKind::Renko { .. } => {
                0.5 * (chart.bounds.width / chart.scaling)
                    - (8.0 * chart.cell_width / chart.scaling)
            }
        };
        Vector::new(x_translation, chart.translation.y)
    }

    fn supports_fit_autoscaling(&self) -> bool {
        true
    }

    fn plot_background(&self) -> Option<Element<'_, Message>> {
        self.indicators[KlineIndicator::LiquidityHeatmap].as_ref()?;
        self.footprint_history
            .liquidity
            .as_ref()
            .map(|runtime| runtime.heatmap.overlay_view())
    }

    fn allows_vertical_navigation_from_fit(&self) -> bool {
        matches!(self.kind, KlineChartKind::Tpo { .. })
    }

    fn is_empty(&self) -> bool {
        match &self.data_source {
            PlotData::TimeBased(timeseries) => timeseries.datapoints.is_empty(),
            PlotData::TickBased(tick_aggr) => tick_aggr.datapoints.is_empty(),
        }
    }
}

impl PlotConstants for KlineChart {
    fn min_scaling(&self) -> f32 {
        self.kind.min_scaling()
    }

    fn max_scaling(&self) -> f32 {
        self.kind.max_scaling()
    }

    fn max_cell_width(&self) -> f32 {
        self.kind.max_cell_width()
    }

    fn min_cell_width(&self) -> f32 {
        self.kind.min_cell_width()
    }

    fn max_cell_height(&self) -> f32 {
        self.kind.max_cell_height()
    }

    fn min_cell_height(&self) -> f32 {
        self.kind.min_cell_height()
    }

    fn default_cell_width(&self) -> f32 {
        self.kind.default_cell_width()
    }
}

pub struct KlineChart {
    chart: ViewState,
    feed: Box<ResolvedFeed>,
    data_source: PlotData<KlineDataPoint>,
    raw_trades: Vec<Trade>,
    /// Compact OHLC history used to seed Renko closes and TPO letters.
    tpo_klines: Box<KlineAggregator>,
    /// Letter-timeframe OHLC history backing the Previous Value Areas overlay.
    pva_klines: Box<KlineAggregator>,
    /// Composite PVA bars changed since the last indicator sync.
    pva_dirty: bool,
    /// UTC day the PVA bar store was anchored at; re-anchored on rollover.
    pva_anchor_day: u64,
    indicators: Box<EnumMap<KlineIndicator, Option<Box<dyn KlineIndicatorImpl>>>>,
    open_interest_sources: Vec<TickerInfo>,
    footprint_history: Box<FootprintHistoryRuntime>,
    /// First live print accepted from each source during this chart session.
    ///
    /// A non-empty candle is not proof that its beginning was backfilled: the
    /// WebSocket can populate the candle before the history planner runs. Keep
    /// the source-specific boundary so each venue can fetch only the missing
    /// prefix without overlapping (and therefore duplicating) live prints.
    live_trade_starts: FxHashMap<TickerInfo, UnixMs>,
    /// Every source in an aggregate visible-history request owns an abort
    /// handle. Keeping only the most recent handle cancels the earlier venue
    /// tasks as soon as the next one is registered.
    fetching_trades: bool,
    trade_fetch_handles: Vec<Handle>,
    active_trade_fetches: Box<FxHashSet<uuid::Uuid>>,
    /// Renko/TPO historical seed completed.
    trade_history_loaded: bool,
    pub(crate) kind: KlineChartKind,
    request_handler: RequestHandler,
    study_configurator: study::Configurator<FootprintStudy>,
    last_tick: Instant,
    /// Data ingestion is immediate; only expensive canvas publication is
    /// coalesced across venue batches.
    last_live_redraw: Instant,
    pending_live_redraw: bool,
    /// Derived only from completed TPO sessions. Keep it out of the canvas draw
    /// path: rebuilding the merged price map on every live publication makes a
    /// 150-profile aggregate chart needlessly CPU-bound.
    tpo_structural_composite: Option<Box<StructuralComposite>>,
    /// UTC day used when the cached completed-session set was last evaluated.
    tpo_structural_anchor_day: u64,
    visual_config: Config,
}

impl KlineChart {
    pub fn new(
        layout: ViewConfig,
        basis: Basis,
        step: PriceStep,
        klines_raw: &[Kline],
        raw_trades: Vec<Trade>,
        enabled_indicators: &[KlineIndicator],
        ticker_info: TickerInfo,
        kind: &KlineChartKind,
        visual_config: Option<Config>,
    ) -> Self {
        let visual_config = visual_config.unwrap_or_default();
        let kind = match kind.clone() {
            KlineChartKind::Tpo { config } => KlineChartKind::Tpo {
                config: config.normalized(),
            },
            kind => kind,
        };
        let feed = ResolvedFeed::direct(ticker_info);
        let basis = if matches!(
            kind,
            KlineChartKind::Renko { .. } | KlineChartKind::Tpo { .. }
        ) {
            Basis::Tick(data::aggr::TickCount(1))
        } else {
            basis
        };

        let mut result = match basis {
            Basis::Time(interval) => {
                let timeseries = TimeSeries::<KlineDataPoint>::new(interval, step, klines_raw)
                    .with_trades(&raw_trades);

                let base_price_y = timeseries.base_price();
                let latest_x = timeseries
                    .latest_timestamp()
                    .map_or(0, |timestamp| timestamp.as_u64());
                let (scale_high, scale_low) = timeseries.price_scale({
                    match &kind {
                        KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. } => 12,
                        KlineChartKind::Candles | KlineChartKind::Renko { .. } => 60,
                    }
                });

                let low_rounded = scale_low.round_to_side_step(true, step);
                let high_rounded = scale_high.round_to_side_step(false, step);

                let y_ticks = Price::steps_between_inclusive(low_rounded, high_rounded, step)
                    .map(|n| n.saturating_sub(1))
                    .unwrap_or(1)
                    .max(1) as f32;

                let cell_width = match &kind {
                    KlineChartKind::Footprint { .. } => kind.default_cell_width(),
                    KlineChartKind::Tpo { .. } => kind.default_cell_width(),
                    KlineChartKind::Candles => 4.0,
                    KlineChartKind::Renko { .. } => kind.default_cell_width(),
                };
                let cell_height = match &kind {
                    KlineChartKind::Footprint { .. } => 800.0 / y_ticks,
                    KlineChartKind::Tpo { .. } => {
                        (800.0 / y_ticks).clamp(kind.min_cell_height(), 12.0)
                    }
                    KlineChartKind::Candles | KlineChartKind::Renko { .. } => 200.0 / y_ticks,
                };

                let mut chart = ViewState::new(
                    basis,
                    step,
                    step.decimal_places(),
                    ticker_info,
                    ViewConfig {
                        splits: layout.splits.clone(),
                        autoscale: Some(Autoscale::FitToVisible),
                    },
                    cell_width,
                    cell_height,
                );
                chart.base_price_y = base_price_y;
                chart.latest_x = latest_x;

                let x_translation = match &kind {
                    KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. } => {
                        0.5 * (chart.bounds.width / chart.scaling)
                            - (chart.cell_width / chart.scaling)
                    }
                    KlineChartKind::Candles | KlineChartKind::Renko { .. } => {
                        0.5 * (chart.bounds.width / chart.scaling)
                            - (8.0 * chart.cell_width / chart.scaling)
                    }
                };
                chart.translation.x = x_translation;

                let data_source = PlotData::TimeBased(timeseries);

                let mut indicators = EnumMap::default();
                for &i in enabled_indicators {
                    if !kind.allows_indicator(i) {
                        continue;
                    }
                    let mut indi = indicator::kline::make_empty(i);
                    if i == KlineIndicator::OpenInterest {
                        indi.configure_open_interest(&[ticker_info]);
                    }
                    if i.needs_trade_history() {
                        indi.configure_footprint_history(&[ticker_info], true);
                    }
                    if i == KlineIndicator::DailyDelta {
                        indi.set_trade_history_lookback(visual_config.daily_delta_days);
                    }
                    if i == KlineIndicator::LargeTrades {
                        indi.set_large_trades_threshold(visual_config.large_trades_min_usd);
                    }
                    indi.rebuild_from_source(&data_source);
                    indicators[i] = Some(indi);
                }

                KlineChart {
                    chart,
                    tpo_klines: Box::new(KlineAggregator::new(&feed)),
                    pva_klines: Box::new(KlineAggregator::new(&feed)),
                    // Force a first sync so a restored PVA overlay populates
                    // without needing to be toggled.
                    pva_dirty: true,
                    pva_anchor_day: day_start(UnixMs::now()),
                    feed: Box::new(feed),
                    visual_config,
                    data_source,
                    raw_trades,
                    indicators: Box::new(indicators),
                    open_interest_sources: vec![ticker_info],
                    footprint_history: Box::new(FootprintHistoryRuntime::new(ticker_info)),
                    live_trade_starts: FxHashMap::default(),
                    fetching_trades: false,
                    trade_fetch_handles: Vec::new(),
                    active_trade_fetches: Box::default(),
                    trade_history_loaded: false,
                    request_handler: RequestHandler::default(),
                    kind: kind.clone(),
                    study_configurator: study::Configurator::new(),
                    last_tick: Instant::now(),
                    last_live_redraw: Instant::now(),
                    pending_live_redraw: false,
                    tpo_structural_composite: None,
                    tpo_structural_anchor_day: day_start(UnixMs::now()),
                }
            }
            Basis::Tick(interval) => {
                let cell_width = match &kind {
                    KlineChartKind::Footprint { .. } => kind.default_cell_width(),
                    KlineChartKind::Tpo { .. } => kind.default_cell_width(),
                    KlineChartKind::Candles => 4.0,
                    KlineChartKind::Renko { .. } => kind.default_cell_width(),
                };
                let cell_height = match &kind {
                    KlineChartKind::Footprint { .. } => 90.0,
                    KlineChartKind::Tpo { .. } => 0.35,
                    KlineChartKind::Candles | KlineChartKind::Renko { .. } => 8.0,
                };

                let mut chart = ViewState::new(
                    basis,
                    step,
                    step.decimal_places(),
                    ticker_info,
                    ViewConfig {
                        splits: layout.splits.clone(),
                        autoscale: Some(Autoscale::FitToVisible),
                    },
                    cell_width,
                    cell_height,
                );

                let x_translation = match &kind {
                    KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. } => {
                        0.5 * (chart.bounds.width / chart.scaling)
                            - (chart.cell_width / chart.scaling)
                    }
                    KlineChartKind::Candles | KlineChartKind::Renko { .. } => {
                        0.5 * (chart.bounds.width / chart.scaling)
                            - (8.0 * chart.cell_width / chart.scaling)
                    }
                };
                chart.translation.x = x_translation;

                let mut tpo_klines = KlineAggregator::new(&feed);
                if matches!(
                    kind,
                    KlineChartKind::Renko { .. } | KlineChartKind::Tpo { .. }
                ) {
                    tpo_klines.insert(feed.primary(), klines_raw);
                }
                let composite_tpo_klines = tpo_klines.composite_klines();

                let data_source = PlotData::TickBased(match &kind {
                    KlineChartKind::Renko { config } => TickAggr::new_renko_seeded(
                        *config,
                        step,
                        &composite_tpo_klines,
                        &raw_trades,
                    ),
                    KlineChartKind::Tpo { config } => {
                        TickAggr::new_tpo_seeded(*config, step, &composite_tpo_klines, &raw_trades)
                    }
                    _ => TickAggr::new(interval, step, &[]),
                });

                let mut indicators = EnumMap::default();
                for &i in enabled_indicators {
                    if !kind.allows_indicator(i) {
                        continue;
                    }
                    let mut indi = indicator::kline::make_empty(i);
                    if i == KlineIndicator::OpenInterest {
                        indi.configure_open_interest(&[ticker_info]);
                    }
                    if i.needs_trade_history() {
                        indi.configure_footprint_history(&[ticker_info], true);
                    }
                    if i == KlineIndicator::DailyDelta {
                        indi.set_trade_history_lookback(visual_config.daily_delta_days);
                    }
                    if i == KlineIndicator::LargeTrades {
                        indi.set_large_trades_threshold(visual_config.large_trades_min_usd);
                    }
                    indi.rebuild_from_source(&data_source);
                    indicators[i] = Some(indi);
                }

                KlineChart {
                    chart,
                    pva_klines: Box::new(KlineAggregator::new(&feed)),
                    // Force a first sync so a restored PVA overlay populates
                    // without needing to be toggled.
                    pva_dirty: true,
                    pva_anchor_day: day_start(UnixMs::now()),
                    feed: Box::new(feed),
                    visual_config,
                    data_source,
                    raw_trades,
                    tpo_klines: Box::new(tpo_klines),
                    indicators: Box::new(indicators),
                    open_interest_sources: vec![ticker_info],
                    footprint_history: Box::new(FootprintHistoryRuntime::new(ticker_info)),
                    live_trade_starts: FxHashMap::default(),
                    fetching_trades: false,
                    trade_fetch_handles: Vec::new(),
                    active_trade_fetches: Box::default(),
                    trade_history_loaded: false,
                    request_handler: RequestHandler::default(),
                    kind: kind.clone(),
                    study_configurator: study::Configurator::new(),
                    last_tick: Instant::now(),
                    last_live_redraw: Instant::now(),
                    pending_live_redraw: false,
                    tpo_structural_composite: None,
                    tpo_structural_anchor_day: day_start(UnixMs::now()),
                }
            }
        };
        result.refresh_tpo_structural_composite(UnixMs::now());
        result
    }

    fn refresh_tpo_structural_composite(&mut self, now: UnixMs) -> bool {
        let next = match &self.kind {
            KlineChartKind::Tpo { config } if config.show_three_day_composite => {
                match &self.data_source {
                    PlotData::TickBased(tick_aggr) => {
                        structural_composite_for_render(&tick_aggr.datapoints, *config, now)
                    }
                    PlotData::TimeBased(_) => None,
                }
            }
            _ => None,
        };
        let changed = self.tpo_structural_composite.as_deref() != next.as_ref();
        self.tpo_structural_composite = next.map(Box::new);
        self.tpo_structural_anchor_day = day_start(now);
        changed
    }

    pub fn set_feed(&mut self, feed: ResolvedFeed) {
        let open_interest_sources = feed.sources().to_vec();
        let existing = self.tpo_klines.composite_klines();
        let mut tpo_klines = KlineAggregator::new(&feed);
        tpo_klines.insert(feed.primary(), &existing);
        // Letter-timeframe bars stay valid across feed swaps; the aggregator
        // only re-scopes which sources it accepts.
        let existing_pva = self.pva_klines.composite_klines();
        let mut pva_klines = KlineAggregator::new(&feed);
        pva_klines.insert(feed.primary(), &existing_pva);
        self.live_trade_starts.retain(|source, _| {
            feed.sources()
                .iter()
                .any(|candidate| candidate.ticker.same_market(&source.ticker))
        });
        self.chart.ticker_info = feed.primary();
        *self.feed = feed;
        *self.tpo_klines = tpo_klines;
        *self.pva_klines = pva_klines;
        self.pva_dirty = true;
        self.configure_open_interest(open_interest_sources);
    }

    pub fn feed(&self) -> &ResolvedFeed {
        &self.feed
    }

    pub fn configure_open_interest(&mut self, sources: Vec<TickerInfo>) {
        if sources.is_empty() {
            return;
        }
        self.open_interest_sources = sources;
        if let Some(indicator) = self.indicators[KlineIndicator::OpenInterest].as_mut() {
            indicator.configure_open_interest(&self.open_interest_sources);
        }
        self.invalidate(None);
    }

    pub fn open_interest_sources(&self) -> &[TickerInfo] {
        &self.open_interest_sources
    }

    pub fn allows_liquidity_heatmap(&self) -> bool {
        matches!(self.chart.basis, Basis::Time(_))
            && matches!(
                self.kind,
                KlineChartKind::Candles | KlineChartKind::Footprint { .. }
            )
    }

    pub fn liquidity_heatmap_sources(&self) -> &[TickerInfo] {
        &self.footprint_history.liquidity_sources
    }

    pub fn configure_liquidity_heatmap(&mut self, sources: Vec<TickerInfo>) {
        if sources.is_empty() {
            return;
        }
        self.footprint_history.liquidity_sources = sources;

        if self.indicators[KlineIndicator::LiquidityHeatmap].is_none()
            || !self.allows_liquidity_heatmap()
        {
            self.footprint_history.liquidity = None;
            return;
        }

        let shared_step = self
            .footprint_history
            .liquidity_sources
            .first()
            .and_then(|source| data::aggregation::AggregateFeedId::for_seed_ticker(source.ticker))
            .map(data::aggregation::AggregateFeedId::price_step)
            .unwrap_or(self.chart.tick_size);
        let display_step = TickMultiplier(10).multiply_step(shared_step);
        let primary = self
            .footprint_history
            .liquidity_sources
            .first()
            .copied()
            .unwrap_or(self.chart.ticker_info);
        let heatmap_config = data::chart::heatmap::Config {
            order_size_filter: self.visual_config.liquidity_heatmap_order_size_filter,
            ..Default::default()
        };

        self.footprint_history.liquidity = Some(Box::new(LiquidityHeatmapRuntime {
            depth: Box::new(DepthAggregator::new(
                self.footprint_history.liquidity_sources.clone(),
                display_step,
            )),
            heatmap: Box::new(HeatmapShader::new_overlay(
                self.chart.basis,
                display_step,
                primary,
                Some(heatmap_config),
            )),
        }));
    }

    pub fn insert_depth(&mut self, source: TickerInfo, depth: &Depth, update_time: UnixMs) {
        let Some(runtime) = self.footprint_history.liquidity.as_mut() else {
            return;
        };
        let Some(composite) = runtime.depth.insert(source, depth, update_time) else {
            return;
        };
        runtime.heatmap.insert_depth(&composite, update_time);
    }

    #[cfg(test)]
    pub(crate) fn liquidity_heatmap_runtime_active(&self) -> bool {
        self.footprint_history.liquidity.is_some()
    }

    #[cfg(test)]
    pub(crate) fn liquidity_heatmap_has_depth_data(&self) -> bool {
        self.footprint_history
            .liquidity
            .as_ref()
            .is_some_and(|runtime| runtime.heatmap.has_depth_data())
    }

    #[cfg(test)]
    pub(crate) fn liquidity_heatmap_order_size_filter(&self) -> Option<f32> {
        self.footprint_history
            .liquidity
            .as_ref()
            .map(|runtime| runtime.heatmap.visual_config().order_size_filter)
    }

    pub fn update_theme(&mut self, theme: &iced_core::Theme) {
        if let Some(runtime) = self.footprint_history.liquidity.as_mut() {
            runtime.heatmap.update_theme(theme);
        }
    }

    pub fn configure_footprint_history(&mut self, sources: Vec<TickerInfo>, aggregate: bool) {
        if sources.is_empty() {
            return;
        }
        self.footprint_history.sources = sources;
        self.footprint_history.aggregate = aggregate;
        self.reconfigure_previous_value_area_sources();
        for kind in [
            KlineIndicator::FootprintHistory,
            KlineIndicator::DailyDelta,
            KlineIndicator::CumulativeDelta,
            KlineIndicator::LargeTrades,
        ] {
            if let Some(indicator) = self.indicators[kind].as_mut() {
                indicator.configure_footprint_history(
                    &self.footprint_history.sources,
                    self.footprint_history.aggregate,
                );
            }
        }
        // Clear the rendered overlay now, but let the dashboard's next tick
        // own and dispatch any history fetch action. Scheduling here would
        // mark the request active and then discard the returned task.
        self.invalidate(None);
    }

    fn uses_trade_history(&self) -> bool {
        self.indicators[KlineIndicator::FootprintHistory].is_some()
            || self.indicators[KlineIndicator::DailyDelta].is_some()
            || self.indicators[KlineIndicator::CumulativeDelta].is_some()
            || self.indicators[KlineIndicator::LargeTrades].is_some()
    }

    fn wants_visible_range_profile(&self) -> bool {
        self.indicators[KlineIndicator::VisibleRangeProfile].is_some()
    }

    /// Live trades (and history fills) should land in each candle's footprint.
    fn wants_bucketed_trades(&self) -> bool {
        matches!(
            self.kind,
            KlineChartKind::Footprint { .. }
                | KlineChartKind::Renko { .. }
                | KlineChartKind::Tpo { .. }
        ) || matches!(self.data_source, PlotData::TickBased(_))
            || self.wants_visible_range_profile()
    }

    /// Fetch executed trades for whatever candles are currently on screen.
    fn wants_visible_trade_fetch(&self) -> bool {
        matches!(self.kind, KlineChartKind::Footprint { .. }) || self.wants_visible_range_profile()
    }

    fn accepts_main_trade_source(&self, source: TickerInfo) -> bool {
        if matches!(
            self.kind,
            KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. }
        ) {
            self.feed
                .sources()
                .iter()
                .any(|candidate| candidate.ticker.same_market(&source.ticker))
        } else {
            source.ticker.same_market(&self.chart.ticker_info.ticker)
        }
    }

    fn footprint_qty_is_quote(&self) -> bool {
        exchange::unit::qty::volume_size_unit() == exchange::SizeUnit::Quote
            || self
                .feed
                .sources()
                .iter()
                .all(|source| source.market_type() == exchange::adapter::MarketKind::InversePerps)
    }

    fn vpvr_group_step(&self) -> PriceStep {
        TickMultiplier(self.visual_config.vpvr_ticks.max(1)).multiply_step(self.feed.price_step())
    }

    fn for_each_trade_history(&mut self, mut f: impl FnMut(&mut dyn KlineIndicatorImpl)) {
        for kind in [
            KlineIndicator::FootprintHistory,
            KlineIndicator::DailyDelta,
            KlineIndicator::CumulativeDelta,
            KlineIndicator::LargeTrades,
        ] {
            if let Some(indicator) = self.indicators[kind].as_mut() {
                f(indicator.as_mut());
            }
        }
    }

    fn load_cached_footprint_day(&mut self, source: TickerInfo, day: UnixMs) -> Option<UnixMs> {
        let mut found_indicator = false;
        let mut all_loaded = true;
        let mut covered_through: Option<UnixMs> = None;
        self.for_each_trade_history(|indicator| {
            found_indicator = true;
            if let Some(through) = indicator.load_cached_footprint_day(source, day) {
                covered_through =
                    Some(covered_through.map_or(through, |current| current.min(through)));
            } else {
                all_loaded = false;
            }
        });
        (found_indicator && all_loaded)
            .then_some(covered_through)
            .flatten()
    }

    fn persist_cached_footprint_day(&mut self, request: FootprintTradeRequest) {
        self.for_each_trade_history(|indicator| {
            indicator.persist_cached_footprint_day(
                request.source,
                request.day_start,
                request.covered_through,
            );
        });
    }

    fn subplot_indicator_count(&self) -> usize {
        self.indicators
            .iter()
            .filter(|(kind, slot)| slot.is_some() && !kind.is_overlay())
            .count()
    }

    pub fn footprint_history_sources(&self) -> &[TickerInfo] {
        &self.footprint_history.sources
    }

    pub fn footprint_history_aggregate(&self) -> bool {
        self.footprint_history.aggregate
    }

    fn active_footprint_history_sources(&self) -> &[TickerInfo] {
        if self.footprint_history.aggregate {
            &self.footprint_history.sources
        } else {
            self.footprint_history.sources.get(..1).unwrap_or(&[])
        }
    }

    fn previous_value_area_feed(&self) -> ResolvedFeed {
        let sources = self.active_footprint_history_sources();
        sources.first().map_or_else(
            || self.feed.as_ref().clone(),
            |primary| ResolvedFeed::from_sources_selected(*primary, sources, None),
        )
    }

    fn reconfigure_previous_value_area_sources(&mut self) {
        let feed = self.previous_value_area_feed();
        if self.pva_klines.sources() == feed.sources() {
            return;
        }

        let timeframe = self
            .visual_config
            .previous_value_area_tpo_config()
            .letter_timeframe();
        for source in self.pva_klines.sources().iter().copied() {
            self.request_handler
                .drop_requests_for_stream(StreamKind::Kline {
                    ticker_info: source,
                    timeframe,
                });
        }
        *self.pva_klines = KlineAggregator::new(&feed);
        self.pva_dirty = true;
    }

    fn sync_time_cursor(&mut self) {
        let PlotData::TimeBased(timeseries) = &self.data_source else {
            return;
        };
        let Some(latest) = timeseries.latest_timestamp() else {
            return;
        };
        self.chart.latest_x = self.chart.latest_x.max(latest.as_u64());
        if let Some((_, dp)) = timeseries.datapoints.last_key_value() {
            self.chart.last_price = Some(PriceInfoLabel::new(dp.kline.close, dp.kline.open));
        }
    }

    pub fn update_latest_kline(&mut self, kline: &Kline) {
        let updated = match self.data_source {
            PlotData::TimeBased(ref mut timeseries) => {
                timeseries.insert_klines(&[*kline]);

                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indi| indi.on_insert_klines(&[*kline], &self.data_source));

                let chart = self.mut_state();

                if kline.time.as_u64() > chart.latest_x {
                    chart.latest_x = kline.time.as_u64();
                }

                chart.last_price = Some(PriceInfoLabel::new(kline.close, kline.open));
                true
            }
            PlotData::TickBased(_) => false,
        };
        if updated {
            // Periodic maintenance intentionally preserves caches. Publish the
            // newest candle through the same bounded live-render path as trades
            // so candle-only panes cannot retain stale geometry indefinitely.
            self.invalidate_live_render();
        }
    }

    fn fetch_footprint_history(&mut self) -> Option<Action> {
        if !self.uses_trade_history() {
            return None;
        }
        let wants_open_interest = self.indicators[KlineIndicator::FootprintHistory].is_some();

        let cutoff = *self
            .footprint_history
            .cutoff
            .get_or_insert_with(UnixMs::now);
        let footprint_days = self.indicators[KlineIndicator::FootprintHistory]
            .is_some()
            .then_some(FOOTPRINT_HISTORY_DAYS as u64);
        let delta_days = self.indicators[KlineIndicator::DailyDelta]
            .is_some()
            .then_some(u64::from(self.visual_config.daily_delta_days.max(1)));
        let cvd_days = self.indicators[KlineIndicator::CumulativeDelta]
            .is_some()
            .then_some(u64::from(
                crate::chart::indicator::kline::cumulative_delta::FETCH_LOOKBACK_DAYS,
            ));
        let large_trades_days = self.indicators[KlineIndicator::LargeTrades]
            .is_some()
            .then_some(u64::from(LARGE_TRADES_LOOKBACK_DAYS));
        let recent_lookback_days = [footprint_days, delta_days, cvd_days, large_trades_days]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(0);
        let recent_day_ranges = footprint_history_day_ranges(cutoff, recent_lookback_days);
        let from = recent_day_ranges
            .last()
            .map(|(start, _)| *start)
            .unwrap_or(cutoff);
        let sources = self.active_footprint_history_sources().to_vec();
        let fetch_mode = trade_fetch_mode();
        let mut specs = Vec::new();

        for source in sources {
            let source_cutoff = self
                .live_trade_starts
                .get(&source)
                .map_or(cutoff, |first_live| first_live.saturating_sub(1));
            let source_day_ranges =
                footprint_history_day_ranges(source_cutoff, recent_lookback_days);
            let trade_history_available = trade_history_available(source, fetch_mode.clone());
            let source_fetch_active = self
                .footprint_history
                .trade_requests
                .values()
                .any(|request| request.source == source);
            if trade_history_available && !source_fetch_active {
                self.for_each_trade_history(|indicator| {
                    indicator.prepare_footprint_history(source, source_cutoff);
                });
                // Keep one UTC-day backfill active per venue. Venues still fetch
                // in parallel, while each venue fills recent profiles first.
                for (start, end) in source_day_ranges.iter() {
                    let cached_through = self.load_cached_footprint_day(source, *start);
                    let Some((fetch_start, fetch_end)) =
                        missing_day_range(*start, *end, cached_through)
                    else {
                        continue;
                    };
                    let range = FetchRange::FootprintHistoryTrades(*start, *end);
                    let stream = StreamKind::Trades {
                        ticker_info: source,
                    };
                    match self.request_handler.add_request(range, Some(stream)) {
                        Ok(Some(req_id)) => {
                            self.footprint_history.trade_requests.insert(
                                req_id,
                                FootprintTradeRequest {
                                    source,
                                    day_start: *start,
                                    covered_through: *end,
                                },
                            );
                            specs.push(FetchSpec {
                                req_id,
                                fetch: FetchRange::FootprintHistoryTrades(fetch_start, fetch_end),
                                stream: Some(stream),
                            });
                            break;
                        }
                        Ok(None) | Err(ReqError::NoData) => continue,
                        Err(ReqError::Overlaps | ReqError::Failed) => break,
                    }
                }
            }

            if wants_open_interest
                && matches!(source.exchange().venue(), Venue::Binance | Venue::Bybit)
                && source.is_perps()
            {
                let range = FetchRange::FootprintHistoryOpenInterest(from, cutoff);
                let stream = StreamKind::Kline {
                    ticker_info: source,
                    timeframe: Timeframe::H1,
                };
                if let Ok(Some(req_id)) = self.request_handler.add_request(range, Some(stream)) {
                    self.footprint_history.oi_requests.insert(req_id);
                    specs.push(FetchSpec {
                        req_id,
                        fetch: range,
                        stream: Some(stream),
                    });
                }
            }
        }

        (!specs.is_empty()).then_some(Action::RequestFetch(specs))
    }

    pub fn kind(&self) -> &KlineChartKind {
        &self.kind
    }

    /// True while Renko/TPO still has historical seed pages to request.
    pub fn needs_seed_backfill(&self) -> bool {
        matches!(
            &self.kind,
            KlineChartKind::Renko { .. } | KlineChartKind::Tpo { .. }
        ) && !self.trade_history_loaded
    }

    fn fetch_seed_klines(&mut self) -> Option<Action> {
        let (is_renko, is_tpo) = match &self.data_source {
            PlotData::TickBased(tick_aggr) => (tick_aggr.is_renko(), tick_aggr.is_tpo()),
            PlotData::TimeBased(_) => return None,
        };

        if is_renko && !self.trade_history_loaded {
            let source = self.feed.primary();
            let latest = UnixMs::now();
            let need_earliest = latest.saturating_sub(RENKO_HISTORY_MS);

            if self.tpo_klines.source_is_complete(source, need_earliest) {
                self.trade_history_loaded = true;
            } else {
                // Strictly older than stored history, matching TPO. Ending a
                // page on the earliest stored bar reused the first REST range,
                // which then sat Pending forever after insert discarded the
                // Action and left the chart with ~16h of 1m closes.
                let page_end = self
                    .tpo_klines
                    .earliest(source)
                    .map(|time| time.saturating_sub(1))
                    .unwrap_or(latest);
                if page_end > need_earliest {
                    let page_ms = RENKO_SEED_TIMEFRAME
                        .to_milliseconds()
                        .saturating_mul(renko_seed_page_bars(source));
                    let page_start = page_end.saturating_sub(page_ms).max(need_earliest);
                    let range = FetchRange::Kline(page_start, page_end);
                    let stream = StreamKind::Kline {
                        ticker_info: source,
                        timeframe: RENKO_SEED_TIMEFRAME,
                    };
                    if let Some(action) =
                        request_fetch_with_stream(&mut self.request_handler, range, Some(stream))
                    {
                        return Some(action);
                    }
                }
            }
        }

        // TPO history: exchange OHLC bars (Sierra/Quantower), not raw trades.
        // At least 150 profiles are bar-seeded so older sessions stay available.
        if is_tpo
            && !self.trade_history_loaded
            && let KlineChartKind::Tpo { config } = &self.kind
        {
            let letter_tf = config.letter_timeframe();
            let latest = UnixMs::now();
            let need_earliest = tpo_history_earliest(*config, latest);
            let page_ms = letter_tf.to_milliseconds().saturating_mul(1_000);

            for source in self.feed.sources().iter().copied() {
                if self.tpo_klines.source_is_complete(source, need_earliest) {
                    continue;
                }

                // Request bars strictly older than this source's
                // existing history. Request identity includes the
                // stream, so equal ranges can run for every venue.
                let page_end = self
                    .tpo_klines
                    .earliest(source)
                    .map(|time| time.saturating_sub(1))
                    .unwrap_or(latest);
                if page_end <= need_earliest {
                    continue;
                }

                let page_start = page_end.saturating_sub(page_ms).max(need_earliest);
                let range = FetchRange::Kline(page_start, page_end);
                let stream = StreamKind::Kline {
                    ticker_info: source,
                    timeframe: letter_tf,
                };
                if let Some(action) =
                    request_fetch_with_stream(&mut self.request_handler, range, Some(stream))
                {
                    return Some(action);
                }
            }

            if self.tpo_klines.all_sources_complete(need_earliest) {
                self.trade_history_loaded = true;
            }
        }

        None
    }

    fn fetch_missing_data(&mut self) -> Option<Action> {
        // Seed the chart's own bar history before trade-history indicators.
        // CVD backfill used to take every tick and leave Renko with one 1m page.
        if let Some(action) = self.fetch_seed_klines() {
            return Some(action);
        }

        let can_fetch_footprint_history = match &self.data_source {
            PlotData::TimeBased(timeseries) => !timeseries.datapoints.is_empty(),
            PlotData::TickBased(_) => true,
        };
        if can_fetch_footprint_history && let Some(action) = self.fetch_footprint_history() {
            return Some(action);
        }

        // Previous Value Areas letter-timeframe bar seeding.
        if let Some(action) = self.fetch_pva_seed_klines() {
            return Some(action);
        }

        if let PlotData::TimeBased(timeseries) = &self.data_source {
            let timeframe_ms = timeseries.interval.to_milliseconds();

            if timeseries.datapoints.is_empty() {
                let latest = chrono::Utc::now().timestamp_millis() as u64;
                let earliest = latest.saturating_sub(450 * timeframe_ms);

                let range = FetchRange::Kline(UnixMs::new(earliest), UnixMs::new(latest));
                if let Some(action) = request_fetch(&mut self.request_handler, range) {
                    return Some(action);
                }
            }

            let (visible_earliest, visible_latest) = self.visible_timerange()?;
            let (kline_earliest, kline_latest) = timeseries.timerange();
            let visible_earliest_ms = UnixMs::new(visible_earliest);
            let visible_latest_ms = UnixMs::new(visible_latest);
            let visible_span = visible_latest.saturating_sub(visible_earliest);
            let prefetch_earliest = visible_earliest.saturating_sub(visible_span);
            let interval = timeseries.interval;
            let suggested_trade_range =
                timeseries.suggest_trade_fetch_range(visible_earliest_ms, visible_latest_ms);
            let check_earliest = UnixMs::new(prefetch_earliest).max(kline_earliest);
            let check_latest = visible_latest_ms.saturating_add(timeframe_ms);
            let missing_kline_keys = timeseries.check_kline_integrity(check_earliest, check_latest);

            // priority 1, initial klines for visible range
            if visible_earliest_ms < kline_earliest {
                let range = FetchRange::Kline(UnixMs::new(prefetch_earliest), kline_earliest);

                if let Some(action) = request_fetch(&mut self.request_handler, range) {
                    return Some(action);
                }
            }

            // priority 2, trades
            if self.wants_visible_trade_fetch() && !self.fetching_trades && is_trade_fetch_enabled()
            {
                let fetch_mode = trade_fetch_mode();
                let specs = self.visible_trade_prefix_fetches(
                    visible_earliest_ms,
                    visible_latest_ms,
                    interval,
                    fetch_mode.clone(),
                );
                if !specs.is_empty() {
                    self.begin_trade_fetches(&specs);
                    return Some(Action::RequestFetch(specs));
                }

                if let Some((fetch_from, fetch_to)) = suggested_trade_range {
                    let range = FetchRange::FootprintTrades(fetch_from, fetch_to);
                    let mut specs = Vec::new();
                    for source in self.feed.sources().iter().copied() {
                        if !trade_history_available(source, fetch_mode.clone()) {
                            continue;
                        }
                        let stream = StreamKind::Trades {
                            ticker_info: source,
                        };
                        if let Ok(Some(req_id)) =
                            self.request_handler.add_request(range, Some(stream))
                        {
                            specs.push(FetchSpec {
                                req_id,
                                fetch: range,
                                stream: Some(stream),
                            });
                        }
                    }
                    if !specs.is_empty() {
                        self.begin_trade_fetches(&specs);
                        return Some(Action::RequestFetch(specs));
                    }
                }
            }

            // priority 3, indicators
            // (e.g. open interest needs external fetch as it's not derived from klines)
            let ctx = indicator::kline::FetchCtx {
                main_chart: &self.chart,
                timeframe: interval,
                visible_earliest: visible_earliest_ms,
                kline_latest,
                prefetch_earliest: UnixMs::new(prefetch_earliest),
            };
            if let Some(indi) = self.indicators[KlineIndicator::OpenInterest].as_mut()
                && let Some(range) = indi.fetch_range(&ctx)
            {
                let mut specs = Vec::new();
                for source in indi.open_interest_sources().iter().copied() {
                    let fetch_timeframe = crate::chart::indicator::kline::open_interest::OpenInterestIndicator::fetch_timeframe_for(
                        source.exchange(),
                        interval,
                    );
                    let stream = StreamKind::Kline {
                        ticker_info: source,
                        timeframe: fetch_timeframe,
                    };
                    if let Ok(Some(req_id)) = self.request_handler.add_request(range, Some(stream))
                    {
                        specs.push(FetchSpec {
                            req_id,
                            fetch: range,
                            stream: Some(stream),
                        });
                    }
                }
                if !specs.is_empty() {
                    return Some(Action::RequestFetch(specs));
                }
            }

            for indi in self
                .indicators
                .values_mut()
                .filter_map(Option::as_mut)
                .filter(|indi| indi.open_interest_sources().is_empty())
            {
                if let Some(range) = indi.fetch_range(&ctx)
                    && let Some(action) = request_fetch(&mut self.request_handler, range)
                {
                    return Some(action);
                }
            }

            // priority 4, missing klines & integrity check
            if let Some(missing_keys) = missing_kline_keys {
                let latest = missing_keys
                    .iter()
                    .max()
                    .unwrap_or(&visible_latest_ms)
                    .saturating_add(timeframe_ms);
                let earliest = missing_keys
                    .iter()
                    .min()
                    .unwrap_or(&visible_earliest_ms)
                    .saturating_sub(timeframe_ms);

                let range = FetchRange::Kline(earliest, latest);
                if let Some(action) = request_fetch(&mut self.request_handler, range) {
                    return Some(action);
                }
            }
        }

        None
    }

    /// Page letter-timeframe OHLC bars for the Previous Value Areas overlay.
    ///
    /// Mirrors the TPO seeding loop: one backward page per tick per source
    /// until the earliest required period is covered, plus a top-up fetch when
    /// time has moved past the newest stored bar (UTC-day rollover).
    fn fetch_pva_seed_klines(&mut self) -> Option<Action> {
        self.indicators[KlineIndicator::PreviousValueArea].as_ref()?;
        let config = self.visual_config.previous_value_area_tpo_config();
        let letter_tf = config.letter_timeframe();
        let now = UnixMs::now();

        // Re-anchor once per UTC day so completed periods keep tracking `now`.
        let today = day_start(now);
        if today != self.pva_anchor_day {
            self.pva_anchor_day = today;
            for source in self.pva_klines.sources().iter().copied() {
                self.request_handler
                    .drop_requests_for_stream(StreamKind::Kline {
                        ticker_info: source,
                        timeframe: letter_tf,
                    });
            }
        }

        let need_earliest = value_area_history_earliest(config, now);
        let page_ms = letter_tf.to_milliseconds().saturating_mul(1_000);

        for source in self.pva_klines.sources().to_vec() {
            match self.pva_klines.latest(source) {
                Some(latest) if latest.as_u64().saturating_add(page_ms) > now.as_u64() => {
                    // Newest bar is fresh; only older pages may be missing.
                }
                latest => {
                    // Top-up (or first page) ending at the newest missing bar.
                    let page_end = latest
                        .map(|time| time.saturating_add(1))
                        .unwrap_or(now)
                        .min(now);
                    if page_end <= need_earliest {
                        continue;
                    }
                    let page_start = page_end.saturating_sub(page_ms).max(need_earliest);
                    let range = FetchRange::Kline(page_start, page_end);
                    let stream = StreamKind::Kline {
                        ticker_info: source,
                        timeframe: letter_tf,
                    };
                    if let Some(action) =
                        request_fetch_with_stream(&mut self.request_handler, range, Some(stream))
                    {
                        return Some(action);
                    }
                }
            }

            if !self.pva_klines.source_is_complete(source, need_earliest) {
                // Backward paging toward the previous year's start.
                let page_end = self
                    .pva_klines
                    .earliest(source)
                    .map(|time| time.saturating_sub(1))
                    .unwrap_or(now);
                if page_end <= need_earliest {
                    continue;
                }
                let page_start = page_end.saturating_sub(page_ms).max(need_earliest);
                let range = FetchRange::Kline(page_start, page_end);
                let stream = StreamKind::Kline {
                    ticker_info: source,
                    timeframe: letter_tf,
                };
                if let Some(action) =
                    request_fetch_with_stream(&mut self.request_handler, range, Some(stream))
                {
                    return Some(action);
                }
            }
        }

        None
    }

    /// True when historical klines on `timeframe` feed the PVA bar store.
    pub fn accepts_pva_seed_klines(&self, timeframe: Timeframe) -> bool {
        self.indicators[KlineIndicator::PreviousValueArea].is_some()
            && timeframe
                == self
                    .visual_config
                    .previous_value_area_tpo_config()
                    .letter_timeframe()
    }

    /// Route fetched letter-timeframe bars into the PVA bar store.
    pub fn insert_pva_seed_klines(
        &mut self,
        req_id: uuid::Uuid,
        source: TickerInfo,
        klines_raw: &[Kline],
    ) {
        if klines_raw.is_empty() {
            self.request_handler.mark_no_data(req_id);
            self.pva_klines.mark_exhausted(source);
        } else {
            self.request_handler.mark_completed(req_id);
            self.pva_klines.insert(source, klines_raw);
            self.pva_dirty = true;
            // Rebuild before the next canvas draw so restored overlays do not
            // stay blank until the 1s pane tick.
            self.sync_previous_value_areas();
        }
        self.invalidate(None);
    }

    /// Push composite PVA bars into the indicator and refresh its caches.
    ///
    /// Called from the periodic tick; cheap unless bars changed or a period
    /// boundary rolled over.
    fn sync_previous_value_areas(&mut self) -> bool {
        if self.indicators[KlineIndicator::PreviousValueArea].is_none() {
            return false;
        }
        let config = self.visual_config.previous_value_area_tpo_config();
        let row_step = previous_value_area_price_step(
            self.active_footprint_history_sources(),
            self.chart.tick_size,
            self.visual_config.previous_value_area_ticks,
        );
        let now = UnixMs::now();
        let dirty = std::mem::take(&mut self.pva_dirty);
        let bars = dirty.then(|| self.pva_klines.composite_klines());
        if let Some(indicator) = self.indicators[KlineIndicator::PreviousValueArea].as_mut() {
            return indicator.sync_value_areas(bars.as_deref(), config, row_step, now);
        }
        false
    }

    pub fn reset_request_handler(&mut self) {
        self.request_handler.drop_non_footprint_history();
        self.reset_trade_fetch_state();
    }

    pub fn reset_trade_fetch_state(&mut self) {
        self.fetching_trades = false;
        self.active_trade_fetches.clear();
        self.trade_fetch_handles.clear();
    }

    fn begin_trade_fetches(&mut self, specs: &[FetchSpec]) {
        if self.active_trade_fetches.is_empty() {
            self.trade_fetch_handles.clear();
        }
        self.active_trade_fetches
            .extend(specs.iter().map(|spec| spec.req_id));
        self.fetching_trades = !self.active_trade_fetches.is_empty();
    }

    fn finish_trade_fetch(&mut self, req_id: uuid::Uuid) {
        if !self.active_trade_fetches.remove(&req_id) {
            return;
        }
        if self.active_trade_fetches.is_empty() {
            self.fetching_trades = false;
            self.trade_fetch_handles.clear();
        }
    }

    fn complete_main_trade_fetch(&mut self, req_id: Option<uuid::Uuid>) {
        if let Some(req_id) = req_id {
            self.request_handler.mark_completed(req_id);
            self.finish_trade_fetch(req_id);
        } else {
            self.reset_trade_fetch_state();
        }
    }

    pub fn is_fetching_trades(&self) -> bool {
        self.fetching_trades
    }

    fn visible_trade_prefix_fetches(
        &mut self,
        visible_earliest: UnixMs,
        visible_latest: UnixMs,
        interval: Timeframe,
        fetch_mode: TradeFetchMode,
    ) -> Vec<FetchSpec> {
        let mut specs = Vec::new();
        let sources = self.feed.sources().to_vec();
        let interval_ms = interval.to_milliseconds();

        for source in sources {
            if !trade_history_available(source, fetch_mode.clone()) {
                continue;
            }
            let Some(first_live) = self.live_trade_starts.get(&source).copied() else {
                continue;
            };
            let fetch_from = first_live.floor_to(interval);
            let fetch_to = first_live.saturating_sub(1);
            let bucket_end = fetch_from.saturating_add(interval_ms);
            if fetch_from >= fetch_to
                || fetch_from > visible_latest
                || bucket_end < visible_earliest
            {
                continue;
            }

            let range = FetchRange::FootprintTrades(fetch_from, fetch_to);
            let stream = StreamKind::Trades {
                ticker_info: source,
            };
            if let Ok(Some(req_id)) = self.request_handler.add_request(range, Some(stream)) {
                specs.push(FetchSpec {
                    req_id,
                    fetch: range,
                    stream: Some(stream),
                });
            }
        }

        specs
    }

    /// Finish a successful trade-history fetch that did not already finalize
    /// via an `is_batches_done` data page (footprint gap fills often end this way).
    pub fn finalize_trade_fetch(&mut self, req_id: uuid::Uuid) {
        self.for_each_trade_history(|indicator| {
            indicator.commit_staged_source_trades(req_id);
        });
        if let Some(request) = self.footprint_history.trade_requests.remove(&req_id) {
            self.persist_cached_footprint_day(request);
            self.request_handler.mark_completed(req_id);
            return;
        }
        self.request_handler.mark_completed(req_id);
        self.finish_trade_fetch(req_id);
        // TPO only seeds trades once; without this flag the chart re-requests the
        // same history window after every capped seed.
        if matches!(
            &self.data_source,
            PlotData::TickBased(tick_aggr) if tick_aggr.is_tpo()
        ) {
            self.trade_history_loaded = true;
        }
    }

    /// Mark a fetch request as failed to unblock re-fetches of the same range.
    pub fn mark_fetch_failed(&mut self, req_id: uuid::Uuid) {
        self.for_each_trade_history(|indicator| {
            indicator.discard_staged_source_trades(req_id);
        });
        self.footprint_history.trade_requests.remove(&req_id);
        self.footprint_history.oi_requests.remove(&req_id);
        // Retry after the handler's cooldown instead of pretending the
        // snapshot completed: a transient failure (rate-limit burst, network
        // blip) must not permanently hole a UTC-day profile. RequestHandler
        // bounds total attempts, so a permanently broken range still cannot
        // retry forever.
        self.request_handler.mark_failed(req_id);
        self.finish_trade_fetch(req_id);
    }

    /// Mark a fetch request as having no data. The source confirmed the
    /// range is empty and it should never be retried.
    pub fn mark_fetch_no_data(&mut self, req_id: uuid::Uuid) {
        self.for_each_trade_history(|indicator| {
            indicator.discard_staged_source_trades(req_id);
        });
        self.footprint_history.trade_requests.remove(&req_id);
        self.footprint_history.oi_requests.remove(&req_id);
        self.request_handler.mark_no_data(req_id);
        self.finish_trade_fetch(req_id);
        if matches!(
            &self.data_source,
            PlotData::TickBased(tick_aggr) if tick_aggr.is_tpo()
        ) {
            self.trade_history_loaded = true;
        }
    }

    pub fn raw_trades(&self) -> Vec<Trade> {
        self.raw_trades.clone()
    }

    pub fn live_trade_starts(&self) -> FxHashMap<TickerInfo, UnixMs> {
        self.live_trade_starts.clone()
    }

    pub fn restore_live_trade_starts(&mut self, starts: FxHashMap<TickerInfo, UnixMs>) {
        self.live_trade_starts = starts;
        let sources = self.feed.sources();
        self.live_trade_starts.retain(|source, _| {
            sources
                .iter()
                .any(|candidate| candidate.ticker.same_market(&source.ticker))
        });
    }

    pub fn set_handle(&mut self, req_id: uuid::Uuid, handle: Handle) {
        // Main visible-history requests can contain one task per selected
        // venue. Retain every handle or registering Bybit/Hyperliquid aborts
        // the Binance task that was registered immediately before it. Route
        // by request identity so a concurrent Daily Delta day fetch cannot be
        // mistaken for (and later aborted with) a VPVR/main-history request.
        if self.active_trade_fetches.contains(&req_id) {
            self.trade_fetch_handles.push(handle);
        } else {
            self.footprint_history.fetch_handles.push(handle);
        }
    }

    /// Forget requests that could not be dispatched (no matching ready stream
    /// yet) so [`Self::fetch_missing_data`] plans them again once streams
    /// resolve, instead of the ranges being suppressed as pending overlaps.
    pub fn release_undispatched_requests(&mut self, ids: &[uuid::Uuid]) {
        for id in ids {
            self.for_each_trade_history(|indicator| {
                indicator.discard_staged_source_trades(*id);
            });
            self.footprint_history.trade_requests.remove(id);
            self.footprint_history.oi_requests.remove(id);
            self.request_handler.remove(*id);
            self.finish_trade_fetch(*id);
        }
    }

    pub fn tick_size(&self) -> PriceStep {
        self.chart.tick_size
    }

    pub fn study_configurator(&self) -> &study::Configurator<FootprintStudy> {
        &self.study_configurator
    }

    pub fn update_study_configurator(&mut self, message: study::Message<FootprintStudy>) {
        let KlineChartKind::Footprint {
            ref mut studies, ..
        } = self.kind
        else {
            return;
        };

        match self.study_configurator.update(message) {
            Some(study::Action::ToggleStudy(study, is_selected)) => {
                if is_selected {
                    let already_exists = studies.iter().any(|s| s.is_same_type(&study));
                    if !already_exists {
                        studies.push(study);
                    }
                } else {
                    studies.retain(|s| !s.is_same_type(&study));
                }
            }
            Some(study::Action::ConfigureStudy(study)) => {
                if let Some(existing_study) = studies.iter_mut().find(|s| s.is_same_type(&study)) {
                    *existing_study = study;
                }
            }
            None => {}
        }

        self.invalidate(None);
    }

    pub fn chart_layout(&self) -> ViewConfig {
        self.chart.layout()
    }

    pub fn visual_config(&self) -> Config {
        self.visual_config
    }

    pub fn set_visual_config(&mut self, visual_config: Config) {
        let lookback_changed =
            self.visual_config.daily_delta_days != visual_config.daily_delta_days;
        let letter_tf_changed = self
            .visual_config
            .previous_value_area_tpo_config()
            .letter_timeframe()
            != visual_config
                .previous_value_area_tpo_config()
                .letter_timeframe();
        let pva_config_changed = letter_tf_changed
            || self.visual_config.previous_value_area_ticks
                != visual_config.previous_value_area_ticks
            || self.visual_config.previous_value_area_block_size
                != visual_config.previous_value_area_block_size
            || self
                .visual_config
                .previous_value_area_session_start_minutes_utc
                != visual_config.previous_value_area_session_start_minutes_utc
            || self.visual_config.previous_value_area_value_area_percent
                != visual_config.previous_value_area_value_area_percent;
        let liquidity_filter_changed = self
            .visual_config
            .liquidity_heatmap_order_size_filter
            .to_bits()
            != visual_config.liquidity_heatmap_order_size_filter.to_bits();
        let mut visual_config = visual_config;
        visual_config.large_trades_min_usd = visual_config.large_trades_min_usd.clamp(
            Config::LARGE_TRADES_MIN_USD_MIN,
            Config::LARGE_TRADES_MIN_USD_MAX,
        );
        let large_trades_threshold_changed = self.visual_config.large_trades_min_usd.to_bits()
            != visual_config.large_trades_min_usd.to_bits();
        self.visual_config = visual_config;
        if lookback_changed {
            self.request_handler = RequestHandler::default();
            self.footprint_history.cutoff = None;
            self.footprint_history.trade_requests.clear();
            self.footprint_history.fetch_handles.clear();
            self.for_each_trade_history(|indicator| {
                indicator.reset_trade_history_backfill();
            });
        } else if pva_config_changed {
            // Letter-timeframe changes invalidate every stored bar; other knob
            // changes only need a rebuild from the existing bars.
            self.request_handler = RequestHandler::default();
            if letter_tf_changed {
                *self.pva_klines = KlineAggregator::new(&self.previous_value_area_feed());
            }
            self.pva_dirty = true;
        }
        if let Some(indicator) = self.indicators[KlineIndicator::DailyDelta].as_mut() {
            indicator.set_trade_history_lookback(self.visual_config.daily_delta_days);
        }
        if large_trades_threshold_changed
            && let Some(indicator) = self.indicators[KlineIndicator::LargeTrades].as_mut()
        {
            indicator.set_large_trades_threshold(self.visual_config.large_trades_min_usd);
        }
        if liquidity_filter_changed && let Some(runtime) = self.footprint_history.liquidity.as_mut()
        {
            let mut config = runtime.heatmap.visual_config();
            config.order_size_filter = self.visual_config.liquidity_heatmap_order_size_filter;
            runtime.heatmap.set_visual_config(config);
        }
        self.chart.cache.clear_all();
        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indi| indi.clear_all_caches());
    }

    pub fn set_cluster_kind(&mut self, new_kind: ClusterKind) {
        if let KlineChartKind::Footprint {
            ref mut clusters, ..
        } = self.kind
        {
            *clusters = new_kind;
        }

        self.invalidate(None);
    }

    pub fn set_cluster_scaling(&mut self, new_scaling: ClusterScaling) {
        if let KlineChartKind::Footprint {
            ref mut scaling, ..
        } = self.kind
        {
            *scaling = new_scaling;
        }

        self.invalidate(None);
    }

    pub fn set_renko_config(&mut self, new_config: RenkoConfig) {
        let normalized = new_config.normalized();
        let KlineChartKind::Renko { config } = &mut self.kind else {
            return;
        };
        *config = normalized;

        self.data_source = PlotData::TickBased(TickAggr::new_renko_seeded(
            normalized,
            self.chart.tick_size,
            &self.tpo_klines.composite_klines(),
            &self.raw_trades,
        ));
        self.chart.last_price = match &self.data_source {
            PlotData::TickBased(tick_aggr) => tick_aggr
                .datapoints
                .last()
                .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open)),
            PlotData::TimeBased(_) => None,
        };

        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indicator| indicator.on_basis_change(&self.data_source));
        self.reset_request_handler();
        self.invalidate(None);
    }

    pub fn set_tpo_config(&mut self, new_config: TpoConfig) {
        let normalized = new_config.normalized();
        let KlineChartKind::Tpo { config } = &mut self.kind else {
            return;
        };
        let history_changed = config.profile_period != normalized.profile_period
            || config.profiles_to_load != normalized.profiles_to_load
            || config.session_start_minutes_utc != normalized.session_start_minutes_utc
            || config.block_size != normalized.block_size
            || config.ticks_per_row != normalized.ticks_per_row;
        let construction_changed = config.profile_period != normalized.profile_period
            || config.session_start_minutes_utc != normalized.session_start_minutes_utc
            || config.block_size != normalized.block_size
            || config.ticks_per_row != normalized.ticks_per_row
            || config.value_area_percent != normalized.value_area_percent
            || config.initial_balance_blocks != normalized.initial_balance_blocks;
        let letter_tf_changed =
            config.block_size.letter_timeframe() != normalized.block_size.letter_timeframe();
        *config = normalized;

        if construction_changed {
            let composite_klines = self.tpo_klines.composite_klines();
            self.data_source = PlotData::TickBased(TickAggr::new_tpo_seeded(
                normalized,
                self.chart.tick_size,
                &composite_klines,
                &self.raw_trades,
            ));
            self.chart.last_price = match &self.data_source {
                PlotData::TickBased(tick_aggr) => tick_aggr
                    .datapoints
                    .last()
                    .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open)),
                PlotData::TimeBased(_) => None,
            };
        }
        if history_changed {
            if letter_tf_changed {
                *self.tpo_klines = KlineAggregator::new(&self.feed);
            }
            self.trade_history_loaded = false;
            self.reset_request_handler();
        }
        self.refresh_tpo_structural_composite(UnixMs::now());
        self.invalidate(None);
    }

    pub fn basis(&self) -> Basis {
        self.chart.basis
    }

    pub fn change_tick_size(&mut self, new_step: PriceStep) {
        let chart = self.mut_state();

        chart.cell_height *= (new_step.units as f32) / (chart.tick_size.units as f32);
        chart.tick_size = new_step;

        match &mut self.data_source {
            PlotData::TickBased(tick_aggr) if tick_aggr.is_tpo() => {
                if let KlineChartKind::Tpo { config } = &self.kind {
                    let composite_klines = self.tpo_klines.composite_klines();
                    self.data_source = PlotData::TickBased(TickAggr::new_tpo_seeded(
                        *config,
                        new_step,
                        &composite_klines,
                        &self.raw_trades,
                    ));
                }
            }
            PlotData::TickBased(tick_aggr) if tick_aggr.is_renko() => {
                // Rebuild from the full minute-close seed so a price-step
                // change re-bricks the whole loaded history instead of
                // collapsing to the current live session.
                if let KlineChartKind::Renko { config } = &self.kind {
                    let composite_klines = self.tpo_klines.composite_klines();
                    self.data_source = PlotData::TickBased(TickAggr::new_renko_seeded(
                        *config,
                        new_step,
                        &composite_klines,
                        &self.raw_trades,
                    ));
                    self.chart.last_price = match &self.data_source {
                        PlotData::TickBased(tick_aggr) => tick_aggr
                            .datapoints
                            .last()
                            .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open)),
                        PlotData::TimeBased(_) => None,
                    };
                }
            }
            PlotData::TickBased(tick_aggr) => {
                tick_aggr.change_tick_size(new_step, &self.raw_trades);
            }
            // Time-based footprints are stored at the feed's base price step
            // and grouped onto `new_step` at draw time (`grouped_to_step`),
            // so switching multipliers only needs the ViewState update above.
            PlotData::TimeBased(_) => {}
        }

        self.refresh_tpo_structural_composite(UnixMs::now());

        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indi| indi.on_ticksize_change(&self.data_source));

        self.invalidate(None);
    }

    pub fn set_basis(&mut self, new_basis: Basis) -> Option<Action> {
        if matches!(
            &self.kind,
            KlineChartKind::Renko { .. } | KlineChartKind::Tpo { .. }
        ) {
            return None;
        }

        let previous_basis = self.chart.basis;

        self.chart.last_price = None;
        self.chart.basis = new_basis;

        match new_basis {
            Basis::Time(interval) => {
                if matches!(previous_basis, Basis::Tick(_)) {
                    self.raw_trades.clear();
                };

                // Store footprint bins on the feed's base tick; the selected
                // tick multiplier is applied when drawing (grouped_to_step).
                let storage_step = self.feed.price_step();
                let timeseries = TimeSeries::<KlineDataPoint>::new(interval, storage_step, &[]);
                self.data_source = PlotData::TimeBased(timeseries);
            }
            Basis::Tick(tick_count) => {
                let trades = if matches!(previous_basis, Basis::Tick(_)) {
                    &self.raw_trades
                } else {
                    self.raw_trades.clear();
                    &vec![]
                };

                let step = self.chart.tick_size;
                let tick_aggr = TickAggr::new(tick_count, step, trades);
                self.data_source = PlotData::TickBased(tick_aggr);
            }
        }

        if self.indicators[KlineIndicator::LiquidityHeatmap].is_some() {
            if matches!(new_basis, Basis::Time(_)) {
                self.configure_liquidity_heatmap(self.footprint_history.liquidity_sources.clone());
            } else {
                self.footprint_history.liquidity = None;
            }
        }

        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indi| indi.on_basis_change(&self.data_source));

        self.live_trade_starts.clear();
        self.reset_request_handler();
        self.invalidate(Some(Instant::now()))
    }

    pub fn studies(&self) -> Option<Vec<FootprintStudy>> {
        match &self.kind {
            KlineChartKind::Footprint { studies, .. } => Some(studies.clone()),
            _ => None,
        }
    }

    pub fn set_studies(&mut self, new_studies: Vec<FootprintStudy>) {
        if let KlineChartKind::Footprint {
            ref mut studies, ..
        } = self.kind
        {
            *studies = new_studies;
        }

        self.invalidate(None);
    }

    pub fn insert_trades(&mut self, source: TickerInfo, buffer: &[Trade]) {
        let history_overlay_updated = self.uses_trade_history();
        if let Some(first_live) = buffer.iter().map(|trade| trade.time).min() {
            self.live_trade_starts
                .entry(source)
                .and_modify(|current| *current = (*current).min(first_live))
                .or_insert(first_live);
        }
        self.for_each_trade_history(|indicator| {
            indicator.on_source_trades(source, buffer, false);
        });

        let main_uses_trades = self.wants_bucketed_trades();
        if !main_uses_trades || !self.accepts_main_trade_source(source) {
            if history_overlay_updated {
                self.invalidate_live_render();
            }
            return;
        }

        // Live stream can run for hours — keep a hard ceiling so TPO/Renko
        // rebuilds and re-seeds never OOM from unbounded raw trade retention.
        const MAX_LIVE_RAW_TRADES: usize = 80_000;
        self.raw_trades.extend_from_slice(buffer);
        if self.raw_trades.len() > MAX_LIVE_RAW_TRADES {
            let excess = self.raw_trades.len() - MAX_LIVE_RAW_TRADES;
            self.raw_trades.drain(..excess);
        }

        match self.data_source {
            PlotData::TickBased(ref mut tick_aggr) => {
                let old_dp_len = tick_aggr.datapoints.len();
                tick_aggr.insert_trades(buffer);
                let completed_session_changed =
                    tick_aggr.is_tpo() && tick_aggr.datapoints.len() != old_dp_len;

                if let Some(last_dp) = tick_aggr.datapoints.last() {
                    self.chart.last_price =
                        Some(PriceInfoLabel::new(last_dp.kline.close, last_dp.kline.open));
                } else {
                    self.chart.last_price = None;
                }

                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indi| indi.on_insert_trades(buffer, old_dp_len, &self.data_source));

                if completed_session_changed {
                    self.refresh_tpo_structural_composite(UnixMs::now());
                }

                self.invalidate_live_render();
            }
            PlotData::TimeBased(ref mut timeseries) => {
                if matches!(self.kind, KlineChartKind::Footprint { .. }) {
                    // Aggregated live tape can arrive before REST klines.
                    // Create the minute/hour buckets from trades so the
                    // footprint is not a blank "Waiting for data" pane.
                    timeseries.insert_trades_or_create_bucket(buffer);
                } else {
                    timeseries.insert_trades_existing_buckets(buffer);
                }
                self.sync_time_cursor();

                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indi| indi.on_insert_trades(buffer, 0, &self.data_source));

                self.invalidate_live_render();
            }
        }
    }

    fn refresh_history_overlay(&mut self, force: bool) {
        const OVERLAY_REFRESH: Duration = Duration::from_millis(400);
        let now = Instant::now();
        let due = self
            .footprint_history
            .last_overlay_redraw
            .is_none_or(|previous| now.duration_since(previous) >= OVERLAY_REFRESH);
        if !force && !due {
            return;
        }
        self.footprint_history.last_overlay_redraw = Some(now);
        if force {
            self.invalidate(None);
        } else {
            self.chart.cache.clear_all();
        }
    }

    pub fn insert_raw_trades(
        &mut self,
        source: TickerInfo,
        raw_trades: Vec<Trade>,
        is_batches_done: bool,
        req_id: Option<uuid::Uuid>,
    ) {
        if req_id.is_some_and(|id| self.footprint_history.trade_requests.contains_key(&id)) {
            if let Some(req_id) = req_id {
                self.for_each_trade_history(|indicator| {
                    indicator.stage_source_trades(req_id, source, &raw_trades);
                });
                if is_batches_done {
                    self.for_each_trade_history(|indicator| {
                        indicator.commit_staged_source_trades(req_id);
                    });
                    self.refresh_history_overlay(true);
                    self.request_handler.mark_completed(req_id);
                }
            }
            return;
        }

        // Source toggles replace the chart while previously dispatched fetch
        // tasks can still deliver a final message. Only accept main-history
        // pages owned by this chart generation and selected by its current
        // feed; otherwise deselected or duplicated venue data leaks back in.
        if req_id.is_some_and(|id| !self.active_trade_fetches.contains(&id))
            || !self.accepts_main_trade_source(source)
        {
            return;
        }

        let is_renko = matches!(
            &self.data_source,
            PlotData::TickBased(tick_aggr) if tick_aggr.is_renko()
        );
        let is_trade_profile = matches!(
            &self.data_source,
            PlotData::TickBased(tick_aggr) if tick_aggr.is_tpo()
        );

        // Renko history is seeded from minute closes. A stale raw-trade seed
        // from an older request must never replace the live chart.
        if is_renko {
            if is_batches_done {
                self.complete_main_trade_fetch(req_id);
            }
            return;
        }

        if matches!(&self.data_source, PlotData::TickBased(_)) && !is_trade_profile {
            if is_batches_done {
                self.complete_main_trade_fetch(req_id);
            }
            return;
        }

        if is_trade_profile {
            // Absolute ceiling for one history seed. Beyond this the chart is
            // usable and the live stream keeps it fresh — do not keep paging.
            const MAX_SEED_TRADES: usize = 80_000;
            const MAX_RAW_TRADES: usize = 80_000;

            if self.trade_history_loaded && !raw_trades.is_empty() {
                // Late pages after we already hit the cap — drop them.
                if is_batches_done {
                    self.complete_main_trade_fetch(req_id);
                }
                return;
            }

            if !raw_trades.is_empty() && self.raw_trades.len() < MAX_SEED_TRADES {
                let room = MAX_SEED_TRADES - self.raw_trades.len();
                let batch = if raw_trades.len() > room {
                    &raw_trades[..room]
                } else {
                    raw_trades.as_slice()
                };

                self.raw_trades.extend_from_slice(batch);
                if self.raw_trades.len() > MAX_RAW_TRADES {
                    let excess = self.raw_trades.len() - MAX_RAW_TRADES;
                    self.raw_trades.drain(..excess);
                }

                match &mut self.data_source {
                    PlotData::TickBased(tick_aggr) => {
                        tick_aggr.insert_trades(batch);
                        self.chart.last_price = tick_aggr
                            .datapoints
                            .last()
                            .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open));
                    }
                    PlotData::TimeBased(_) => {}
                }

                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indicator| indicator.on_insert_trades(batch, 0, &self.data_source));
            }

            // Hit the seed cap: abort further history paging and go live.
            if self.raw_trades.len() >= MAX_SEED_TRADES {
                // Dropping the abort handle cancels the in-flight fetch task.
                self.complete_main_trade_fetch(req_id);
                self.trade_history_loaded = true;
                self.invalidate(Some(Instant::now()));
                return;
            }

            if is_batches_done {
                self.complete_main_trade_fetch(req_id);
                self.trade_history_loaded = true;
            }

            // Redraw on completion, or lightly while seeding so the UI stays alive.
            if is_batches_done || self.raw_trades.len().is_multiple_of(20_000) {
                self.refresh_tpo_structural_composite(UnixMs::now());
                self.invalidate(Some(Instant::now()));
            }
            return;
        }

        // Skip unnecessary work when the batch is empty. The true "no data
        // at all" case is handled separately via mark_fetch_no_data in the
        // dashboard.
        if raw_trades.is_empty() {
            if is_batches_done {
                self.complete_main_trade_fetch(req_id);
            }
            return;
        }

        if let PlotData::TimeBased(ref mut timeseries) = self.data_source {
            if matches!(self.kind, KlineChartKind::Footprint { .. }) {
                timeseries.insert_trades_or_create_bucket(&raw_trades);
            } else {
                timeseries.insert_trades_existing_buckets(&raw_trades);
            }
            self.sync_time_cursor();
        }

        self.raw_trades.extend_from_slice(&raw_trades);

        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indi| indi.on_insert_trades(&raw_trades, 0, &self.data_source));

        if is_batches_done {
            self.complete_main_trade_fetch(req_id);
        }

        self.invalidate(None);
    }

    pub fn insert_hist_klines(
        &mut self,
        req_id: uuid::Uuid,
        source: TickerInfo,
        klines_raw: &[Kline],
    ) {
        match &mut self.data_source {
            PlotData::TimeBased(timeseries) => {
                timeseries.insert_klines(klines_raw);
                timeseries.fill_empty_footprints_from_trades(&self.raw_trades);
                self.sync_time_cursor();

                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indi| indi.on_insert_klines(klines_raw, &self.data_source));

                if klines_raw.is_empty() {
                    self.request_handler.mark_no_data(req_id);
                } else {
                    self.request_handler.mark_completed(req_id);
                }
                self.invalidate(None);
            }
            PlotData::TickBased(_) if matches!(self.kind, KlineChartKind::Renko { .. }) => {
                if klines_raw.is_empty() {
                    self.request_handler.mark_no_data(req_id);
                    self.tpo_klines.mark_exhausted(source);
                    self.trade_history_loaded = true;
                    // Redraw only. Planning the next page here would register it
                    // as Pending with no dispatcher, which suppressed every later
                    // retry of that range (Overlaps) and froze seed history.
                    self.invalidate(None);
                    return;
                }

                self.tpo_klines.insert(source, klines_raw);
                if let KlineChartKind::Renko { config } = &self.kind {
                    self.data_source = PlotData::TickBased(TickAggr::new_renko_seeded(
                        *config,
                        self.chart.tick_size,
                        &self.tpo_klines.composite_klines(),
                        &self.raw_trades,
                    ));
                    self.chart.last_price = match &self.data_source {
                        PlotData::TickBased(tick_aggr) => tick_aggr
                            .datapoints
                            .last()
                            .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open)),
                        PlotData::TimeBased(_) => None,
                    };
                }
                self.request_handler.mark_completed(req_id);
                let need_earliest = UnixMs::now().saturating_sub(RENKO_HISTORY_MS);
                self.trade_history_loaded =
                    self.tpo_klines.source_is_complete(source, need_earliest);
                self.indicators
                    .values_mut()
                    .filter_map(Option::as_mut)
                    .for_each(|indicator| indicator.on_basis_change(&self.data_source));
                self.invalidate(None);
            }
            PlotData::TickBased(tick_aggr) if matches!(self.kind, KlineChartKind::Tpo { .. }) => {
                if klines_raw.is_empty() {
                    // No more historical bars available — stop paging.
                    self.request_handler.mark_no_data(req_id);
                    self.tpo_klines.mark_exhausted(source);
                    if let KlineChartKind::Tpo { config } = &self.kind {
                        let need_earliest = tpo_history_earliest(*config, UnixMs::now());
                        self.trade_history_loaded =
                            self.tpo_klines.all_sources_complete(need_earliest);
                    }
                    self.invalidate(None);
                    return;
                }

                self.tpo_klines.insert(source, klines_raw);
                let page_start = klines_raw
                    .iter()
                    .map(|kline| kline.time)
                    .min()
                    .expect("non-empty TPO history page");
                let page_end = klines_raw
                    .iter()
                    .map(|kline| kline.time)
                    .max()
                    .expect("non-empty TPO history page");
                let KlineChartKind::Tpo { config } = &self.kind else {
                    unreachable!("TPO branch must retain TPO config");
                };
                let first_session = config.profile_start(page_start);
                let last_session = config.profile_start(page_end);
                let session_end = last_session
                    .saturating_add(config.normalized().profile_period.millis())
                    .saturating_sub(1);
                let canonical_sessions = self
                    .tpo_klines
                    .composite_klines_in_range(first_session, session_end);
                let affected_sessions = klines_raw
                    .iter()
                    .map(|kline| config.profile_start(kline.time))
                    .collect::<Vec<_>>();

                // Re-materialize only the touched complete sessions from the
                // canonical source-aware bar store. This can retract corrected
                // rows and makes venue page arrival order irrelevant while
                // preserving all profiles outside the page's sessions.
                tick_aggr.replace_tpo_sessions_from_canonical(
                    &affected_sessions,
                    &canonical_sessions,
                    &self.raw_trades,
                );
                self.chart.last_price = tick_aggr
                    .datapoints
                    .last()
                    .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open));

                self.request_handler.mark_completed(req_id);

                // Done when we cover the configured profile history depth.
                if let KlineChartKind::Tpo { config } = &self.kind {
                    let need_earliest = tpo_history_earliest(*config, UnixMs::now());
                    self.trade_history_loaded = self.tpo_klines.all_sources_complete(need_earliest);
                }

                self.refresh_tpo_structural_composite(UnixMs::now());
                self.invalidate(None);
            }
            PlotData::TickBased(_) => {
                self.request_handler.mark_completed(req_id);
            }
        }
    }

    /// True when this chart accepts historical klines even on a tick basis.
    pub fn accepts_tick_kline_seed(&self, timeframe: exchange::Timeframe) -> bool {
        matches!(&self.kind, KlineChartKind::Renko { .. } if timeframe == RENKO_SEED_TIMEFRAME)
            || matches!(&self.kind, KlineChartKind::Tpo { config } if config.letter_timeframe() == timeframe)
    }

    pub fn insert_open_interest(
        &mut self,
        source: TickerInfo,
        req_id: Option<uuid::Uuid>,
        oi_data: &[OIData],
    ) {
        if req_id.is_some_and(|id| self.footprint_history.oi_requests.contains(&id)) {
            if let Some(indicator) = self.indicators[KlineIndicator::FootprintHistory].as_mut() {
                indicator.on_source_open_interest(source, oi_data);
            }
            if let Some(req_id) = req_id {
                if oi_data.is_empty() {
                    self.request_handler.mark_no_data(req_id);
                } else {
                    self.request_handler.mark_completed(req_id);
                }
            }
            self.invalidate(None);
            return;
        }

        let Some(indi) = self.indicators[KlineIndicator::OpenInterest].as_mut() else {
            return;
        };
        if !indi.open_interest_sources().contains(&source) {
            return;
        }
        if let Some(req_id) = req_id {
            if oi_data.is_empty() {
                self.request_handler.mark_no_data(req_id);
            } else {
                self.request_handler.mark_completed(req_id);
            }
        }

        indi.on_source_open_interest(source, oi_data);
    }

    fn calc_qty_scales(
        &self,
        earliest: u64,
        latest: u64,
        highest: Price,
        lowest: Price,
        step: PriceStep,
        cluster_kind: ClusterKind,
    ) -> f64 {
        let rounded_highest = highest.round_to_side_step(false, step).add_steps(1, step);
        let rounded_lowest = lowest.round_to_side_step(true, step).add_steps(-1, step);

        match &self.data_source {
            PlotData::TimeBased(timeseries) => timeseries
                .max_qty_ts_range(
                    cluster_kind,
                    UnixMs::new(earliest),
                    UnixMs::new(latest),
                    rounded_highest,
                    rounded_lowest,
                    step,
                )
                .to_f64(),
            PlotData::TickBased(tick_aggr) => {
                let earliest = earliest as usize;
                let latest = latest as usize;

                tick_aggr
                    .max_qty_idx_range(
                        cluster_kind,
                        earliest,
                        latest,
                        rounded_highest,
                        rounded_lowest,
                    )
                    .to_f64()
            }
        }
    }

    pub fn last_update(&self) -> Instant {
        self.last_tick
    }

    pub fn has_pending_live_redraw(&self) -> bool {
        self.pending_live_redraw
    }

    fn invalidate_live_render(&mut self) {
        self.invalidate_live_render_at(Instant::now());
    }

    fn invalidate_live_render_at(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_live_redraw) >= LIVE_REDRAW_INTERVAL {
            self.flush_live_render(now);
        } else {
            self.pending_live_redraw = true;
        }
    }

    fn flush_live_render(&mut self, now: Instant) {
        // Every indicator data hook invalidates its own geometry. Clearing all
        // subplot caches here rebuilt unchanged indicators (notably OI) on
        // every trade batch, so live publication only clears the shared main
        // chart canvas. Full viewport/config/history invalidation still clears
        // every indicator through `invalidate`.
        self.invalidate_render(None, false);
        self.last_live_redraw = now;
    }

    /// Run periodic fetch/overlay maintenance without discarding chart and
    /// indicator geometry when neither data nor the viewport changed.
    pub fn maintain(&mut self, now: Instant) -> Option<Action> {
        if self.pending_live_redraw
            && now.saturating_duration_since(self.last_live_redraw) >= LIVE_REDRAW_INTERVAL
        {
            self.flush_live_render(now);
        }
        self.last_tick = now;

        let wall_now = UnixMs::now();
        if self.tpo_structural_anchor_day != day_start(wall_now)
            && self.refresh_tpo_structural_composite(wall_now)
        {
            self.chart.cache.main.clear();
        }

        if self.sync_previous_value_areas() {
            // Previous Value Areas are painted on the main canvas. Their
            // source/config revision changed, but axes and subplot caches did
            // not.
            self.chart.cache.main.clear();
        }

        let overlay_action = if self.indicators[KlineIndicator::LiquidityHeatmap].is_some() {
            self.footprint_history
                .liquidity
                .as_mut()
                .and_then(|runtime| runtime.heatmap.sync_overlay(&self.chart, now))
        } else {
            self.footprint_history.liquidity = None;
            None
        };

        // A newly-enabled GPU overlay has no palette until the dashboard
        // services RequestPalette. Give that one-time request priority.
        overlay_action.or_else(|| self.fetch_missing_data())
    }

    pub fn invalidate(&mut self, now: Option<Instant>) -> Option<Action> {
        self.invalidate_render(now, true)
    }

    fn invalidate_render(
        &mut self,
        now: Option<Instant>,
        clear_indicator_caches: bool,
    ) -> Option<Action> {
        let chart = &mut self.chart;

        if let Some(autoscale) = chart.layout.autoscale {
            match autoscale {
                super::Autoscale::CenterLatest => {
                    let x_translation = match &self.kind {
                        KlineChartKind::Footprint { .. } | KlineChartKind::Tpo { .. } => {
                            0.5 * (chart.bounds.width / chart.scaling)
                                - (chart.cell_width / chart.scaling)
                        }
                        KlineChartKind::Candles | KlineChartKind::Renko { .. } => {
                            0.5 * (chart.bounds.width / chart.scaling)
                                - (8.0 * chart.cell_width / chart.scaling)
                        }
                    };
                    chart.translation.x = x_translation;

                    let calculate_target_y = |kline: exchange::Kline| -> f32 {
                        let y_low = chart.price_to_y(kline.low);
                        let y_high = chart.price_to_y(kline.high);
                        let y_close = chart.price_to_y(kline.close);

                        let mut target_y_translation = -(y_low + y_high) / 2.0;

                        if chart.bounds.height > f32::EPSILON && chart.scaling > f32::EPSILON {
                            let visible_half_height = (chart.bounds.height / chart.scaling) / 2.0;

                            let view_center_y_centered = -target_y_translation;

                            let visible_y_top = view_center_y_centered - visible_half_height;
                            let visible_y_bottom = view_center_y_centered + visible_half_height;

                            let padding = chart.cell_height;

                            if y_close < visible_y_top {
                                target_y_translation = -(y_close - padding + visible_half_height);
                            } else if y_close > visible_y_bottom {
                                target_y_translation = -(y_close + padding - visible_half_height);
                            }
                        }
                        target_y_translation
                    };

                    chart.translation.y = self.data_source.latest_y_midpoint(calculate_target_y);
                }
                super::Autoscale::FitToVisible => {
                    let visible_region = chart.visible_region(chart.bounds.size());
                    let (start_interval, end_interval) = chart.interval_range(&visible_region);

                    if let Some((lowest, highest)) = self
                        .data_source
                        .visible_price_range(start_interval, end_interval)
                    {
                        let chart_height = chart.bounds.height;
                        let tick_size = chart.tick_size.to_f32_lossy();

                        if chart_height > f32::EPSILON && tick_size > 0.0 {
                            let (fit_lowest, fit_highest) = match &self.kind {
                                KlineChartKind::Footprint { .. } => {
                                    const MIN_VISIBLE_FOOTPRINT_ROWS: f32 = 14.0;
                                    let (profile_low, profile_high) = self
                                        .data_source
                                        .visible_footprint_price_range(start_interval, end_interval)
                                        .map(|(low, high)| {
                                            (low.to_f32_lossy(), high.to_f32_lossy())
                                        })
                                        .unwrap_or((lowest, highest));
                                    let midpoint = (profile_low + profile_high) * 0.5;
                                    let profile_span =
                                        (profile_high - profile_low).max(0.0) + tick_size;
                                    let fitted_span =
                                        profile_span.max(tick_size * MIN_VISIBLE_FOOTPRINT_ROWS);
                                    (midpoint - fitted_span * 0.5, midpoint + fitted_span * 0.5)
                                }
                                KlineChartKind::Tpo { config } => {
                                    const MIN_VISIBLE_TPO_ROWS: f32 = 16.0;
                                    let row_step = tick_size * config.ticks_per_row as f32;
                                    let (profile_low, profile_high) = self
                                        .data_source
                                        .visible_tpo_price_range(start_interval, end_interval)
                                        .map(|(low, high)| {
                                            (low.to_f32_lossy(), high.to_f32_lossy())
                                        })
                                        .unwrap_or((lowest, highest));
                                    let midpoint = (profile_low + profile_high) * 0.5;
                                    let profile_span = (profile_high - profile_low) + row_step;
                                    let fitted_span =
                                        profile_span.max(row_step * MIN_VISIBLE_TPO_ROWS);
                                    (midpoint - fitted_span * 0.5, midpoint + fitted_span * 0.5)
                                }
                                KlineChartKind::Candles | KlineChartKind::Renko { .. } => {
                                    (lowest, highest)
                                }
                            };

                            let visible_span = (fit_highest - fit_lowest).max(tick_size);
                            let base_padding = visible_span * 0.05; // 5% padding on top and bottom

                            let mut top_padding = base_padding;
                            let mut bottom_padding = base_padding;

                            if let KlineChartKind::Footprint { .. } = self.kind {
                                let provisional_span = visible_span + top_padding + bottom_padding;
                                if provisional_span > 0.0 {
                                    let provisional_cell_height =
                                        (chart_height * tick_size) / provisional_span;

                                    let outer_padding = price_padding_from_pixels(
                                        provisional_cell_height,
                                        tick_size,
                                    );

                                    top_padding += outer_padding;
                                    bottom_padding += outer_padding;

                                    if self.visual_config.show_footprint_summary {
                                        bottom_padding =
                                            bottom_padding.max(FootprintSummaryLayout::padding(
                                                provisional_cell_height,
                                                chart.scaling,
                                                tick_size,
                                            ));
                                    }
                                }
                            }

                            let padded_span = visible_span + top_padding + bottom_padding;
                            if padded_span > 0.0 {
                                let fitted = (chart_height * tick_size) / padded_span;
                                // Keep fitted height inside zoom limits so the
                                // right-scale Y drag/scroll can move either way.
                                chart.cell_height = fitted.clamp(
                                    self.kind.min_cell_height(),
                                    self.kind.max_cell_height(),
                                );
                                chart.base_price_y = Price::from_f32(fit_highest + top_padding);
                                chart.translation.y = -chart_height / 2.0;
                            }
                        }
                    }
                }
            }
        }

        chart.cache.clear_all();
        if clear_indicator_caches {
            for indi in self.indicators.values_mut().filter_map(Option::as_mut) {
                indi.clear_all_caches();
            }
        }
        self.pending_live_redraw = false;
        self.last_live_redraw = Instant::now();

        let overlay_action = if self.indicators[KlineIndicator::LiquidityHeatmap].is_some() {
            self.footprint_history
                .liquidity
                .as_mut()
                .and_then(|runtime| {
                    runtime
                        .heatmap
                        .sync_overlay(&self.chart, now.unwrap_or_else(Instant::now))
                })
        } else {
            self.footprint_history.liquidity = None;
            None
        };

        if let Some(t) = now {
            self.last_tick = t;
            self.sync_previous_value_areas();
            // A newly-enabled GPU overlay has no palette until the dashboard
            // services RequestPalette. Give that one-time request priority so
            // ongoing history fetches cannot leave the overlay permanently blank.
            overlay_action.or_else(|| self.fetch_missing_data())
        } else {
            overlay_action
        }
    }

    pub fn toggle_indicator(&mut self, indicator: KlineIndicator) {
        let is_enabled = self.indicators[indicator].is_some();
        if !is_enabled
            && (!self.kind.allows_indicator(indicator)
                || (indicator == KlineIndicator::LiquidityHeatmap
                    && !self.allows_liquidity_heatmap()))
        {
            return;
        }

        let prev_indi_count = self.subplot_indicator_count();

        if is_enabled {
            self.indicators[indicator] = None;
            if indicator == KlineIndicator::LiquidityHeatmap {
                // Drop the depth aggregator and GPU heatmap immediately when
                // the overlay is disabled. Stream ownership is released by
                // the pane in the same update.
                self.footprint_history.liquidity.take();
            }
        } else {
            let mut box_indi = indicator::kline::make_empty(indicator);
            if indicator == KlineIndicator::OpenInterest {
                box_indi.configure_open_interest(&self.open_interest_sources);
            }
            if indicator.needs_trade_history() {
                self.request_handler = RequestHandler::default();
                self.footprint_history.cutoff = None;
                // Dropping these abort handles cancels every in-flight venue
                // backfill. Forget their ownership records as well; otherwise
                // `fetch_footprint_history` keeps those venues permanently
                // marked active and never retries them for the new indicator.
                let stale_requests = self
                    .footprint_history
                    .trade_requests
                    .keys()
                    .copied()
                    .collect::<Vec<_>>();
                self.for_each_trade_history(|existing| {
                    for req_id in &stale_requests {
                        existing.discard_staged_source_trades(*req_id);
                    }
                });
                self.footprint_history.trade_requests.clear();
                self.footprint_history.oi_requests.clear();
                self.footprint_history.fetch_handles.clear();
                box_indi.configure_footprint_history(
                    &self.footprint_history.sources,
                    self.footprint_history.aggregate,
                );
                if indicator == KlineIndicator::DailyDelta {
                    box_indi.set_trade_history_lookback(self.visual_config.daily_delta_days);
                }
                if indicator == KlineIndicator::LargeTrades {
                    box_indi.set_large_trades_threshold(self.visual_config.large_trades_min_usd);
                }
            }
            if indicator == KlineIndicator::PreviousValueArea {
                self.pva_dirty = true;
            }
            box_indi.rebuild_from_source(&self.data_source);
            self.indicators[indicator] = Some(box_indi);
            if indicator == KlineIndicator::LiquidityHeatmap {
                self.configure_liquidity_heatmap(self.footprint_history.liquidity_sources.clone());
            }
        }

        if let Some(main_split) = self.chart.layout.splits.first() {
            let current_indi_count = self.subplot_indicator_count();
            self.chart.layout.splits = data::util::calc_panel_splits(
                *main_split,
                current_indi_count,
                Some(prev_indi_count),
            );
        }
    }
}

impl canvas::Program<Message> for KlineChart {
    type State = Interaction;

    fn update(
        &self,
        interaction: &mut Interaction,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        super::canvas_interaction(self, interaction, event, bounds, cursor)
    }

    fn draw(
        &self,
        interaction: &Interaction,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let chart = self.state();

        if chart.bounds.width == 0.0 {
            return vec![];
        }

        let bounds_size = bounds.size();
        let palette = theme.extended_palette();

        let klines = chart.cache.main.draw(renderer, bounds_size, |frame| {
            let center = Vector::new(bounds.width / 2.0, bounds.height / 2.0);

            frame.translate(center);
            frame.scale(chart.scaling);
            frame.translate(chart.translation);

            let region = chart.visible_region(frame.size());
            let (earliest, latest) = chart.interval_range(&region);

            let price_to_y = |price| chart.price_to_y(price);
            let interval_to_x = |interval| chart.interval_to_x(interval);

            match &self.kind {
                KlineChartKind::Footprint {
                    clusters,
                    scaling,
                    studies,
                } => {
                    let (highest, lowest) = chart.price_range(&region);

                    let max_cluster_qty = self.calc_qty_scales(
                        earliest,
                        latest,
                        highest,
                        lowest,
                        chart.tick_size,
                        *clusters,
                    );

                    // Never let the candle shrink below ~2.5 screen px so
                    // body/wick stay visible when panned/zoomed away.
                    let candle_width = (chart.cell_width * 0.1).max(2.5 / chart.scaling.max(0.1));
                    let content_spacing = ContentGaps::from_view(candle_width, chart.scaling);

                    let imbalance = studies.iter().find_map(|study| {
                        if let FootprintStudy::Imbalance {
                            threshold,
                            color_scale,
                            ignore_zeros,
                        } = study
                        {
                            Some((*threshold, *color_scale, *ignore_zeros))
                        } else {
                            None
                        }
                    });

                    let cell_layout = FootprintCellLayout {
                        cell_w: chart.cell_width,
                        cell_h: chart.cell_height,
                        candle_w: candle_width,
                        pal: palette,
                        cluster: *clusters,
                        gaps: content_spacing,
                    };

                    draw_all_npocs(
                        &self.data_source,
                        frame,
                        price_to_y,
                        interval_to_x,
                        &cell_layout,
                        studies,
                        earliest,
                        latest,
                        imbalance.is_some(),
                    );

                    let qty_is_quote = self.footprint_qty_is_quote();
                    let (visible_high, visible_low) = chart.price_range(&region);
                    render_data_source(
                        &self.data_source,
                        frame,
                        earliest,
                        latest,
                        interval_to_x,
                        |frame, x_position, kline, trades| {
                            let grouped = trades.grouped_to_step(self.tick_size());
                            let visible_max_notional = if qty_is_quote {
                                max_cluster_qty
                            } else {
                                max_cluster_qty * kline.close.to_f64().max(1.0)
                            };
                            let individual_max_notional =
                                max_cluster_notional(&grouped, cell_layout.cluster, qty_is_quote);
                            let cluster_scaling = effective_notional_scale(
                                *scaling,
                                visible_max_notional,
                                individual_max_notional,
                            );

                            draw_clusters(
                                frame,
                                price_to_y,
                                x_position,
                                &cell_layout,
                                chart.scaling,
                                cluster_scaling,
                                qty_is_quote,
                                self.tick_size(),
                                self.visual_config.show_footprint_summary,
                                imbalance,
                                kline,
                                &grouped,
                                visible_high,
                                visible_low,
                            );
                        },
                    );
                }
                KlineChartKind::Candles => {
                    let candle_width = chart.cell_width * 0.8;

                    render_data_source(
                        &self.data_source,
                        frame,
                        earliest,
                        latest,
                        interval_to_x,
                        |frame, x_position, kline, _| {
                            draw_candle_dp(
                                frame,
                                price_to_y,
                                candle_width,
                                palette,
                                x_position,
                                kline,
                                true,
                            );
                        },
                    );
                }
                KlineChartKind::Renko { .. } => {
                    let candle_width = chart.cell_width * 0.8;

                    render_data_source(
                        &self.data_source,
                        frame,
                        earliest,
                        latest,
                        interval_to_x,
                        |frame, x_position, kline, _| {
                            draw_candle_dp(
                                frame,
                                price_to_y,
                                candle_width,
                                palette,
                                x_position,
                                kline,
                                self.visual_config.show_renko_wicks,
                            );
                        },
                    );
                }
                KlineChartKind::Tpo { config } => {
                    let row_step = config.row_step(chart.tick_size);
                    let (visible_high, visible_low) = chart.price_range(&region);
                    draw_tpo_profiles(
                        &self.data_source,
                        frame,
                        earliest,
                        latest,
                        interval_to_x,
                        price_to_y,
                        chart.cell_width,
                        chart.cell_height * config.ticks_per_row as f32,
                        chart.scaling,
                        palette,
                        *config,
                        row_step,
                        visible_high,
                        visible_low,
                        region.x + region.width,
                        self.tpo_structural_composite.as_deref(),
                    );
                }
            }

            if let Some(indicator) = self.indicators[KlineIndicator::DailyDelta].as_ref() {
                let ticks = self.visual_config.daily_delta_ticks.max(1);
                let active_sources = self.active_footprint_history_sources();
                let min_tick = shared_history_price_step(active_sources, chart.tick_size);
                let group_step = TickMultiplier(ticks).multiply_step(min_tick);
                indicator.draw_overlay(
                    frame,
                    chart,
                    &self.data_source,
                    palette,
                    region,
                    group_step,
                );
            }

            if let Some(indicator) = self.indicators[KlineIndicator::PreviousValueArea].as_ref() {
                let group_step = previous_value_area_price_step(
                    self.active_footprint_history_sources(),
                    chart.tick_size,
                    self.visual_config.previous_value_area_ticks,
                );
                indicator.draw_overlay(
                    frame,
                    chart,
                    &self.data_source,
                    palette,
                    region,
                    group_step,
                );
            }

            if let Some(indicator) = self.indicators[KlineIndicator::LargeTrades].as_ref() {
                indicator.draw_overlay(
                    frame,
                    chart,
                    &self.data_source,
                    palette,
                    region,
                    chart.tick_size,
                );
            }

            if let Some(indicator) = self.indicators[KlineIndicator::VisibleRangeProfile].as_ref() {
                let group_step = self.vpvr_group_step();
                indicator.draw_overlay(
                    frame,
                    chart,
                    &self.data_source,
                    palette,
                    region,
                    group_step,
                );
            }

            chart.draw_last_price_line(frame, palette, region);
        });

        let crosshair = chart.cache.crosshair.draw(renderer, bounds_size, |frame| {
            let visible_region = chart.visible_region(bounds_size);
            let visible_range = chart.interval_range(&visible_region);

            if let Some(cursor_position) = cursor.position_in(bounds) {
                let (_, rounded_aggregation) =
                    chart.draw_crosshair(frame, theme, bounds_size, cursor_position, interaction);

                draw_crosshair_tooltip(
                    &self.data_source,
                    &chart.ticker_info,
                    frame,
                    palette,
                    chart.basis,
                    Some(rounded_aggregation),
                    visible_range,
                );
            } else if self.visual_config.data_labels_always_visible {
                draw_crosshair_tooltip(
                    &self.data_source,
                    &chart.ticker_info,
                    frame,
                    palette,
                    chart.basis,
                    None,
                    visible_range,
                );
            }

            if let Some(cursor_position) = cursor.position_in(bounds) {
                let center = Vector::new(bounds.width / 2.0, bounds.height / 2.0);
                frame.translate(center);
                frame.scale(chart.scaling);
                frame.translate(chart.translation);
                let cursor_chart = Point::new(
                    (cursor_position.x - bounds.width / 2.0) / chart.scaling - chart.translation.x,
                    (cursor_position.y - bounds.height / 2.0) / chart.scaling - chart.translation.y,
                );
                if let Some(indicator) = self.indicators[KlineIndicator::LargeTrades].as_ref() {
                    indicator.draw_hover(
                        frame,
                        chart,
                        &self.data_source,
                        palette,
                        visible_region,
                        cursor_chart,
                    );
                }
                if let Some(indicator) =
                    self.indicators[KlineIndicator::VisibleRangeProfile].as_ref()
                {
                    indicator.draw_hover(
                        frame,
                        chart,
                        &self.data_source,
                        palette,
                        visible_region,
                        cursor_chart,
                    );
                }
            }
        });

        vec![klines, crosshair]
    }

    fn mouse_interaction(
        &self,
        interaction: &Interaction,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match interaction {
            Interaction::Panning { .. } => mouse::Interaction::Grabbing,
            Interaction::Zoomin { .. } => mouse::Interaction::ZoomIn,
            Interaction::None | Interaction::Ruler { .. } => {
                if cursor.is_over(bounds) {
                    mouse::Interaction::Crosshair
                } else {
                    mouse::Interaction::default()
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tpo_profiles(
    data_source: &PlotData<KlineDataPoint>,
    frame: &mut canvas::Frame,
    earliest: u64,
    latest: u64,
    interval_to_x: impl Fn(u64) -> f32,
    price_to_y: impl Fn(Price) -> f32,
    cell_width: f32,
    cell_height: f32,
    scaling: f32,
    palette: &Extended,
    config: TpoConfig,
    row_step: PriceStep,
    visible_high: Price,
    visible_low: Price,
    visible_right: f32,
    structural_composite: Option<&StructuralComposite>,
) {
    let PlotData::TickBased(tick_aggr) = data_source else {
        return;
    };

    if !cell_width.is_finite()
        || !cell_height.is_finite()
        || cell_width <= 0.0
        || cell_height <= 0.0
        || !scaling.is_finite()
        || scaling <= 0.0
    {
        return;
    }

    let start_idx = earliest as usize;
    let end_idx = latest as usize;
    let visible_profiles = prepare_tpo_render(
        &tick_aggr.datapoints,
        start_idx,
        end_idx,
        config,
        cell_width,
        scaling,
        row_step,
        visible_high,
        visible_low,
        structural_composite,
    );

    // Paint extensions first so every profile and its labels remain legible on
    // top. Tick-chart indices are offsets from the newest profile, while the
    // backing vector is oldest-to-newest; the helper below performs that
    // conversion before looking for the first future tag.
    if config.show_single_prints {
        let scale = scaling.max(0.1);
        let min_chart_px = 1.0 / scale;
        let row_body_h = (cell_height * 0.94).max(min_chart_px);
        let half_body = row_body_h * 0.5;
        let line_width = min_chart_px.max(0.5);
        let extension_fill = palette.warning.base.color.scale_alpha(0.14);
        let extension_line = palette.warning.base.color.scale_alpha(0.42);

        for prepared in &visible_profiles {
            let profile_right = interval_to_x(prepared.offset as u64) + prepared.half_width;

            for extension in &prepared.extensions {
                let price = extension.price;
                let tag_x = extension
                    .tag
                    .map(|tag| interval_to_x(tag.offset as u64) - tag.half_width)
                    .unwrap_or(visible_right)
                    .min(visible_right);
                let width = tag_x - profile_right;
                if !width.is_finite() || width <= 0.0 {
                    continue;
                }

                let y = price_to_y(price);
                if !y.is_finite() {
                    continue;
                }
                frame.fill_rectangle(
                    Point::new(profile_right, y - half_body),
                    Size::new(width, row_body_h),
                    extension_fill,
                );
                frame.stroke(
                    &Path::line(Point::new(profile_right, y), Point::new(tag_x, y)),
                    Stroke::with_color(
                        Stroke {
                            width: line_width,
                            ..Default::default()
                        },
                        extension_line,
                    ),
                );
            }
        }
    }

    for prepared in visible_profiles {
        draw_tpo_profile(
            frame,
            &price_to_y,
            interval_to_x(prepared.offset as u64),
            cell_width,
            cell_height,
            scaling,
            palette,
            config,
            prepared.profile,
            prepared.max_row_count,
            prepared.half_width * 2.0,
            prepared.single_print_bounds,
            row_step,
            visible_high,
            visible_low,
            prepared.opacity,
        );
    }

    if let Some(composite) = structural_composite {
        draw_structural_composite(
            frame,
            &interval_to_x,
            &price_to_y,
            &tick_aggr.datapoints,
            composite,
            cell_width,
            cell_height,
            scaling,
            palette,
            visible_high,
            visible_low,
        );
    }
}

fn structural_composite_for_render(
    datapoints: &[data::aggr::ticks::TickAccumulation],
    config: TpoConfig,
    now: UnixMs,
) -> Option<StructuralComposite> {
    let profiles = datapoints
        .iter()
        .filter_map(|datapoint| datapoint.tpo.as_ref())
        .collect::<Vec<_>>();
    active_three_day_composite(&profiles, config, now)
}

#[derive(Clone, Copy)]
struct TpoFutureTag {
    offset: usize,
    half_width: f32,
}

#[derive(Clone, Copy)]
struct TpoSinglePrintExtension {
    price: Price,
    tag: Option<TpoFutureTag>,
}

struct PreparedTpoProfile<'a> {
    offset: usize,
    profile: &'a TpoProfile,
    max_row_count: usize,
    half_width: f32,
    single_print_bounds: Option<(Price, Price)>,
    extensions: Vec<TpoSinglePrintExtension>,
    opacity: f32,
}

#[allow(clippy::too_many_arguments)]
fn draw_structural_composite(
    frame: &mut canvas::Frame,
    interval_to_x: &impl Fn(u64) -> f32,
    price_to_y: &impl Fn(Price) -> f32,
    datapoints: &[data::aggr::ticks::TickAccumulation],
    composite: &StructuralComposite,
    cell_width: f32,
    cell_height: f32,
    scaling: f32,
    palette: &Extended,
    visible_high: Price,
    visible_low: Price,
) {
    let offset_for = |matches: &dyn Fn(&TpoProfile) -> bool| {
        datapoints
            .iter()
            .position(|datapoint| datapoint.tpo.as_ref().is_some_and(matches))
            .and_then(|raw| datapoints.len().checked_sub(raw.saturating_add(1)))
    };
    let Some(oldest_offset) = offset_for(&|profile| profile.start == composite.start) else {
        return;
    };
    let Some(newest_offset) = offset_for(&|profile| profile.end == composite.end) else {
        return;
    };

    let oldest_x = interval_to_x(oldest_offset as u64);
    let newest_x = interval_to_x(newest_offset as u64);
    let left = oldest_x.min(newest_x) - cell_width * 0.48;
    let right = oldest_x.max(newest_x) + cell_width * 0.48;
    let width = right - left;
    if !left.is_finite() || !right.is_finite() || width <= 0.0 {
        return;
    }

    let scale = scaling.max(0.1);
    let min_chart_px = 1.0 / scale;
    let balance_high_y = price_to_y(composite.balance_high);
    let balance_low_y = price_to_y(composite.balance_low);
    let top = balance_high_y.min(balance_low_y);
    let bottom = balance_high_y.max(balance_low_y);
    let height = bottom - top;
    let color = palette.primary.base.color;
    if top.is_finite() && bottom.is_finite() && height > 0.0 {
        frame.fill_rectangle(
            Point::new(left, top),
            Size::new(width, height),
            color.scale_alpha(0.045),
        );
        frame.stroke(
            &Path::rectangle(Point::new(left, top), Size::new(width, height)),
            Stroke::with_color(
                Stroke {
                    width: min_chart_px,
                    ..Default::default()
                },
                color.scale_alpha(0.55),
            ),
        );
    }

    let max_count = composite.rows.values().copied().max().unwrap_or(1).max(1);
    let histogram_left = left + width * 0.04;
    let histogram_width = width * 0.92;
    let row_height = (cell_height * 0.84).max(min_chart_px);
    let (visible_low, visible_high) = if visible_low <= visible_high {
        (visible_low, visible_high)
    } else {
        (visible_high, visible_low)
    };
    for (price, count) in &composite.rows {
        if *price < visible_low || *price > visible_high {
            continue;
        }
        let y = price_to_y(*price);
        if !y.is_finite() {
            continue;
        }
        let bar_width = (histogram_width * (*count as f32 / max_count as f32)).max(min_chart_px);
        let in_value_area =
            *price >= composite.value_area_low && *price <= composite.value_area_high;
        let alpha = if *price == composite.poc {
            0.82
        } else if in_value_area {
            0.52
        } else {
            0.22
        };
        frame.fill_rectangle(
            Point::new(histogram_left, y - row_height * 0.5),
            Size::new(bar_width, row_height),
            color.scale_alpha(alpha),
        );
    }

    let line = |alpha: f32, extra_width: f32| {
        Stroke::with_color(
            Stroke {
                width: min_chart_px + extra_width / scale,
                ..Default::default()
            },
            color.scale_alpha(alpha),
        )
    };
    for (price, label) in [
        (composite.value_area_high, "CVAH"),
        (composite.value_area_low, "CVAL"),
    ] {
        let y = price_to_y(price);
        if y.is_finite() {
            frame.stroke(
                &Path::line(Point::new(left, y), Point::new(right, y)),
                line(0.82, 1.0),
            );
            if width * scale >= 100.0 {
                draw_cluster_text(
                    frame,
                    label,
                    Point::new(right - 3.0 / scale, y),
                    10.0 / scale,
                    color,
                    Alignment::End,
                    Alignment::Center,
                );
            }
        }
    }
    let poc_y = price_to_y(composite.poc);
    if poc_y.is_finite() {
        frame.stroke(
            &Path::line(Point::new(left, poc_y), Point::new(right, poc_y)),
            line(0.95, 0.0),
        );
    }
    if top.is_finite() && width * scale >= 120.0 {
        draw_cluster_text(
            frame,
            &format!("3D BAL · {}D", composite.profile_count),
            Point::new(left + 4.0 / scale, top + 4.0 / scale),
            11.0 / scale,
            color,
            Alignment::Start,
            Alignment::Start,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_tpo_render<'a>(
    datapoints: &'a [data::aggr::ticks::TickAccumulation],
    start_idx: usize,
    end_idx: usize,
    config: TpoConfig,
    cell_width: f32,
    scaling: f32,
    row_step: PriceStep,
    visible_high: Price,
    visible_low: Price,
    structural_composite: Option<&StructuralComposite>,
) -> Vec<PreparedTpoProfile<'a>> {
    if end_idx < start_idx {
        return Vec::new();
    }

    // Match the old paint cap exactly: it counted offset slots before dropping
    // non-TPO datapoints. Profiles newer than the visible range still need to be
    // visited because they can terminate an older profile's extension rail.
    const MAX_PROFILES_PER_FRAME: usize = TpoConfig::MAX_HISTORY_PROFILES as usize;
    let paint_end = end_idx.min(start_idx.saturating_add(MAX_PROFILES_PER_FRAME - 1));
    let low = visible_low.min(visible_high);
    let high = visible_low.max(visible_high);
    let row_pad = row_step.units.max(0);
    let extension_low = Price::from_units(low.units.saturating_sub(row_pad));
    let extension_high = Price::from_units(high.units.saturating_add(row_pad));

    // While walking newest-to-oldest, the latest value stored for a price is
    // exactly the nearest later profile that tags it. This replaces one future
    // profile scan per single-print row with one pass over all relevant rows.
    let mut nearest_future: FxHashMap<Price, TpoFutureTag> = FxHashMap::default();
    let mut prepared = Vec::new();

    for (offset, datapoint) in datapoints.iter().rev().enumerate() {
        if offset > paint_end {
            break;
        }
        let Some(profile) = datapoint.tpo.as_ref() else {
            continue;
        };
        let in_structural_composite =
            structural_composite.is_some_and(|composite| composite.contains_profile(profile));
        let max_row_count = tpo_profile_max_row_count(profile);
        let half_width = 0.5 * tpo_profile_width(config, max_row_count, cell_width, scaling);

        if offset >= start_idx {
            let single_print_bounds = if config.show_single_prints && !in_structural_composite {
                profile.single_print_bounds()
            } else {
                None
            };
            let extensions = if let Some((single_low, single_high)) = single_print_bounds {
                profile
                    .rows
                    .iter()
                    .filter(|(price, row)| {
                        row.count() == 1
                            && **price > single_low
                            && **price < single_high
                            && **price >= extension_low
                            && **price <= extension_high
                    })
                    .map(|(price, _)| TpoSinglePrintExtension {
                        price: *price,
                        tag: nearest_future.get(price).copied(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            prepared.push(PreparedTpoProfile {
                offset,
                profile,
                max_row_count,
                half_width,
                single_print_bounds,
                extensions,
                opacity: if in_structural_composite { 0.20 } else { 1.0 },
            });
        }

        if config.show_single_prints {
            let tag = TpoFutureTag { offset, half_width };
            for price in profile.rows.keys() {
                nearest_future.insert(*price, tag);
            }
        }
    }

    prepared
}

fn tpo_profile_max_row_count(profile: &TpoProfile) -> usize {
    profile
        .rows
        .values()
        .map(|row| row.count())
        .max()
        .unwrap_or(1)
        .max(1)
}

/// Reference lookup retained only for equivalence tests.
#[cfg(test)]
fn first_future_tpo_tag_offset(
    datapoints: &[data::aggr::ticks::TickAccumulation],
    current_offset: usize,
    price: Price,
) -> Option<usize> {
    let current_raw = datapoints
        .len()
        .checked_sub(current_offset.checked_add(1)?)?;
    let future_start = current_raw.checked_add(1)?;
    let future_position = datapoints
        .get(future_start..)?
        .iter()
        .position(|datapoint| {
            datapoint
                .tpo
                .as_ref()
                .is_some_and(|profile| profile.rows.contains_key(&price))
        })?;
    let tag_raw = future_start.checked_add(future_position)?;
    datapoints.len().checked_sub(tag_raw.checked_add(1)?)
}

/// Full on-screen width of a TPO profile silhouette. Shared by the profile
/// renderer and single-print extension rails so both agree where a profile
/// column begins and ends.
fn tpo_profile_width(
    config: TpoConfig,
    max_row_count: usize,
    cell_width: f32,
    scaling: f32,
) -> f32 {
    let scale = scaling.max(0.1);
    let min_chart_px = 1.0 / scale;
    // Keep daily profiles separated while giving each TPO enough width to be
    // read as a letter or distinct block instead of a narrow gray silhouette.
    let profile_width_fraction = if config.profile_period == TpoProfilePeriod::Day {
        0.90
    } else {
        0.96
    };
    let available_width = (cell_width * profile_width_fraction).max(min_chart_px);
    let block_width = (available_width / max_row_count.max(1) as f32).max(min_chart_px * 0.5);
    block_width * max_row_count.max(1) as f32
}

#[allow(clippy::too_many_arguments)]
fn draw_tpo_profile(
    frame: &mut canvas::Frame,
    price_to_y: &impl Fn(Price) -> f32,
    x_position: f32,
    cell_width: f32,
    cell_height: f32,
    scaling: f32,
    palette: &Extended,
    config: TpoConfig,
    profile: &TpoProfile,
    max_row_count: usize,
    profile_width: f32,
    single_print_bounds: Option<(Price, Price)>,
    row_step: PriceStep,
    visible_high: Price,
    visible_low: Price,
    opacity: f32,
) {
    if profile.rows.is_empty() {
        return;
    }

    let max_row_count = max_row_count.max(1);

    // Profile width always follows the period column. Never cap letter width by
    // row height — FitToVisible / large price ranges make cell_height tiny, and
    // that used to collapse the whole silhouette into a hairline.
    let scale = scaling.max(0.1);
    let min_chart_px = 1.0 / scale;
    if !profile_width.is_finite() || profile_width <= 0.0 {
        return;
    }
    let block_width = (profile_width / max_row_count as f32).max(min_chart_px * 0.5);
    if !block_width.is_finite() || block_width <= 0.0 {
        return;
    }
    let profile_width = block_width * max_row_count as f32;
    let left = x_position - profile_width / 2.0;

    // Screen-space sizes (pixels) drive LOD so zoom-in actually reveals letters.
    let screen_block_w = block_width * scale;
    let screen_row_h = cell_height * scale;
    let line_width = min_chart_px.max(0.5);
    // Fill more of each price row so the profile remains legible at normal zoom.
    let row_body_h = (cell_height * 0.94).max(min_chart_px);
    let half_body = row_body_h * 0.5;

    let show_letters = match config.display_style {
        TpoDisplayStyle::Letters => screen_block_w >= 5.0 && screen_row_h >= 6.0,
        TpoDisplayStyle::Blocks => false,
        TpoDisplayStyle::Auto => screen_block_w >= 6.0 && screen_row_h >= 7.0,
    };
    // One bar per price row when individual letters would be sub-pixel noise.
    let compact_row_bars =
        !show_letters && (screen_block_w < 2.5 || max_row_count > 64 || screen_row_h < 2.0);

    let open_row = price_to_row(profile.open, row_step);
    // TPO is time-at-price rather than order-flow delta, so use the session's
    // open-to-close direction while sharing Delta History's visual language.
    let (buy_color, sell_color) = delta_history_colors(palette);
    let base_profile_color = if profile.close >= profile.open {
        buy_color
    } else {
        sell_color
    };
    let opacity = opacity.clamp(0.0, 1.0);
    let profile_color = base_profile_color.scale_alpha(opacity);
    let profile_strong =
        mix_color(base_profile_color, palette.background.base.text, 0.82).scale_alpha(opacity);
    let profile_muted =
        mix_color(base_profile_color, palette.background.base.color, 0.32).scale_alpha(opacity);
    // Warning is the theme's yellow/orange semantic color. Keep the single
    // print ink and its full-row band deliberately faint while remaining
    // independent of bullish/bearish profile direction.
    let single_print_color = |alpha: f32| palette.warning.base.color.scale_alpha(alpha * opacity);
    let y_pad = (cell_height * 2.0).max(4.0 / scale);

    let y_hi = price_to_y(visible_high);
    let y_lo = price_to_y(visible_low);
    let view_top = y_hi.min(y_lo) - y_pad;
    let view_bot = y_hi.max(y_lo) + y_pad;

    const MAX_DRAWN_ROWS: usize = 4_000;
    let mut drawn_rows = 0usize;

    for (price, row) in &profile.rows {
        let y = price_to_y(*price);
        if !y.is_finite() || y < view_top || y > view_bot {
            continue;
        }
        if drawn_rows >= MAX_DRAWN_ROWS {
            break;
        }
        drawn_rows = drawn_rows.saturating_add(1);

        let in_value_area = *price >= profile.value_area_low && *price <= profile.value_area_high;
        let is_poc = *price == profile.poc;
        let is_single = config.show_single_prints
            && row.count() == 1
            && single_print_bounds.is_some_and(|(low, high)| *price > low && *price < high);

        if config.show_poc && is_poc {
            frame.fill_rectangle(
                Point::new(left, y - half_body * 1.15),
                Size::new(profile_width, half_body * 2.3),
                profile_color.scale_alpha(0.18),
            );
        }

        // Faint full-width band behind single prints so un-auctioned territory
        // stands out even when letters collapse into bars.
        if is_single {
            frame.fill_rectangle(
                Point::new(left, y - half_body),
                Size::new(profile_width, half_body * 2.0),
                single_print_color(0.14),
            );
        }

        if compact_row_bars {
            let color = if is_single {
                single_print_color(0.72)
            } else if is_poc && config.show_poc {
                profile_strong
            } else if config.show_value_area && in_value_area {
                profile_color
            } else {
                profile_muted
            };
            let row_alpha = if is_single { 1.0 } else { 0.9 };
            let bar_w = (block_width * row.count() as f32).max(line_width);
            frame.fill_rectangle(
                Point::new(left, y - half_body),
                Size::new(bar_w, half_body * 2.0),
                color.scale_alpha(row_alpha),
            );
            continue;
        }

        for (ordinal, block) in row.blocks.iter().enumerate() {
            let x = left + (ordinal as f32 + 0.5) * block_width;
            let base_color = if is_single {
                single_print_color(0.72)
            } else if is_poc && config.show_poc {
                profile_strong
            } else if config.show_value_area && in_value_area {
                profile_color
            } else {
                profile_muted
            };
            // Keep both color zones clear while retaining a subtle bracket
            // progression. Single prints stay fully visible.
            let time_fade = if is_single {
                1.0
            } else {
                0.85 + 0.15 * (f32::from(*block % 8) / 7.0)
            };
            let color = base_color.scale_alpha(time_fade);

            if show_letters {
                // Glyph size tracks the letter cell; floor at a readable size
                // so letters never shrink into invisibility once LOD shows them.
                let glyph = (block_width * 0.96)
                    .min(cell_height * 0.94)
                    .max(9.0 / scale)
                    .min(block_width.max(cell_height));
                draw_cluster_text(
                    frame,
                    &block_letter(*block).to_string(),
                    Point::new(x, y),
                    glyph,
                    color,
                    Alignment::Center,
                    Alignment::Center,
                );
            } else {
                frame.fill_rectangle(
                    Point::new(x - block_width * 0.42, y - half_body),
                    Size::new((block_width * 0.84).max(line_width), half_body * 2.0),
                    color,
                );
            }
        }
    }

    let profile_right = left + profile_width;
    let line = |color: Color| {
        Stroke::with_color(
            Stroke {
                width: line_width,
                ..Default::default()
            },
            color,
        )
    };
    let value_area_line = |color: Color| {
        Stroke::with_color(
            Stroke {
                // Exactly one screen pixel thicker than normal profile lines.
                width: line_width + 1.0 / scale,
                ..Default::default()
            },
            color,
        )
    };

    // Labels once the column is wide enough on screen.
    let show_level_labels = cell_width * scale >= 64.0 && screen_row_h >= 4.0;
    let label_size = (cell_height * 0.55)
        .clamp(9.0 / scale, 14.0 / scale)
        .max(9.0 / scale);

    if config.show_value_area {
        let available_gap = (cell_width - profile_width).max(12.0 / scale);
        let level_extension = (20.0 / scale).min(available_gap * 0.5);
        let level_left = left - level_extension;
        let level_right = profile_right + level_extension;
        for (price, label) in [
            (profile.value_area_high, "VAH"),
            (profile.value_area_low, "VAL"),
        ] {
            let y = price_to_y(price);
            frame.stroke(
                &Path::line(Point::new(level_left, y), Point::new(level_right, y)),
                value_area_line(profile_color.scale_alpha(0.9)),
            );
            if show_level_labels {
                draw_cluster_text(
                    frame,
                    label,
                    Point::new(level_right + 3.0 / scale, y),
                    label_size,
                    profile_strong,
                    Alignment::Start,
                    Alignment::Center,
                );
            }
        }
    }

    if config.show_poc {
        let y = price_to_y(profile.poc);
        frame.stroke(
            &Path::line(Point::new(left, y), Point::new(profile_right, y)),
            line(profile_strong),
        );
        if show_level_labels {
            draw_cluster_text(
                frame,
                "POC",
                Point::new(profile_right + 2.0 / scale, y),
                label_size,
                profile_strong,
                Alignment::Start,
                Alignment::Center,
            );
        }
    }

    if config.show_initial_balance {
        let ib_x = left - 5.0 / scale;
        let ib_high_y = price_to_y(profile.initial_balance_high);
        let ib_low_y = price_to_y(profile.initial_balance_low);
        let ib_color = palette.warning.base.color.scale_alpha(0.85 * opacity);
        frame.stroke(
            &Path::line(Point::new(ib_x, ib_high_y), Point::new(ib_x, ib_low_y)),
            line(ib_color),
        );
        let cap = 4.0 / scale;
        for y in [ib_high_y, ib_low_y] {
            frame.stroke(
                &Path::line(Point::new(ib_x, y), Point::new(ib_x + cap, y)),
                line(ib_color),
            );
        }
    }

    // Keep the compact open tick, but omit the red close arrow that read as a
    // detached red dot at lower zoom levels.
    let marker_size = (block_width * 0.55)
        .max(cell_height * 0.35)
        .max(2.5 / scale);
    let open_y = price_to_y(open_row);
    let open_color = palette.success.base.color.scale_alpha(0.9 * opacity);

    frame.stroke(
        &Path::line(
            Point::new(left - marker_size, open_y),
            Point::new(left + marker_size * 0.35, open_y),
        ),
        line(open_color),
    );
}

fn draw_footprint_kline(
    frame: &mut canvas::Frame,
    price_to_y: impl Fn(Price) -> f32,
    x_position: f32,
    candle_width: f32,
    kline: &Kline,
    palette: &Extended,
) {
    let y_open = price_to_y(kline.open);
    let y_high = price_to_y(kline.high);
    let y_low = price_to_y(kline.low);
    let y_close = price_to_y(kline.close);

    let is_up = kline.close >= kline.open;
    let body_color = if is_up {
        palette.success.base.color
    } else {
        palette.danger.base.color
    };
    let wick_color = if is_up {
        palette.success.strong.color
    } else {
        palette.danger.strong.color
    };

    let body_h = (y_open - y_close).abs().max(candle_width * 0.18);
    let wick_w = (candle_width * 0.34).max(1.0);
    frame.fill_rectangle(
        Point::new(x_position - wick_w / 2.0, y_high.min(y_low)),
        Size::new(wick_w, (y_high - y_low).abs()),
        wick_color.scale_alpha(0.95),
    );
    frame.fill_rectangle(
        Point::new(x_position - candle_width / 2.0, y_open.min(y_close)),
        Size::new(candle_width, body_h),
        body_color,
    );
}

fn draw_candle_dp(
    frame: &mut canvas::Frame,
    price_to_y: impl Fn(Price) -> f32,
    candle_width: f32,
    palette: &Extended,
    x_position: f32,
    kline: &Kline,
    show_wicks: bool,
) {
    let y_open = price_to_y(kline.open);
    let y_high = price_to_y(kline.high);
    let y_low = price_to_y(kline.low);
    let y_close = price_to_y(kline.close);

    let body_color = if kline.close >= kline.open {
        palette.success.base.color
    } else {
        palette.danger.base.color
    };
    frame.fill_rectangle(
        Point::new(x_position - (candle_width / 2.0), y_open.min(y_close)),
        Size::new(candle_width, (y_open - y_close).abs()),
        body_color,
    );

    if show_wicks {
        let wick_color = if kline.close >= kline.open {
            palette.success.base.color
        } else {
            palette.danger.base.color
        };
        frame.fill_rectangle(
            Point::new(x_position - (candle_width / 8.0), y_high),
            Size::new(candle_width / 4.0, (y_high - y_low).abs()),
            wick_color,
        );
    }
}

fn render_data_source<F>(
    data_source: &PlotData<KlineDataPoint>,
    frame: &mut canvas::Frame,
    earliest: u64,
    latest: u64,
    interval_to_x: impl Fn(u64) -> f32,
    draw_fn: F,
) where
    F: Fn(&mut canvas::Frame, f32, &Kline, &KlineTrades),
{
    match data_source {
        PlotData::TickBased(tick_aggr) => {
            let earliest = earliest as usize;
            let latest = latest as usize;

            // The final Renko datapoint is the unconfirmed projection.
            // Omitting it keeps every visible Renko body fixed-size while the
            // last-price line continues to show the live market.
            let skip = usize::from(tick_aggr.is_renko());
            let base = tick_aggr.datapoints.len().saturating_sub(1 + skip);

            // `earliest`/`latest` count backwards from the newest drawn brick;
            // convert to absolute datapoint indices and slice so a redraw only
            // touches the visible window even on weeks of bricks.
            let range = base
                .checked_sub(earliest)
                .map(|upper| (base.saturating_sub(latest), upper))
                .filter(|(lower, upper)| lower <= upper);

            if let Some((lower, upper)) = range {
                // X positions must use each datapoint's true offset from the
                // newest brick, not the position inside the visible slice, or
                // panning back in time would drag every rendered brick along
                // with the viewport and blank out the chart.
                tick_aggr.datapoints[lower..=upper]
                    .iter()
                    .rev()
                    .zip(earliest..=latest)
                    .for_each(|(accumulation, offset)| {
                        let x_position = interval_to_x(offset as u64);

                        draw_fn(
                            frame,
                            x_position,
                            &accumulation.kline,
                            &accumulation.footprint,
                        );
                    });
            }
        }
        PlotData::TimeBased(timeseries) => {
            if latest < earliest {
                return;
            }

            timeseries
                .datapoints
                .range(UnixMs::new(earliest)..=UnixMs::new(latest))
                .for_each(|(timestamp, dp)| {
                    let x_position = interval_to_x(timestamp.as_u64());

                    draw_fn(frame, x_position, &dp.kline, &dp.footprint);
                });
        }
    }
}

fn draw_all_npocs(
    data_source: &PlotData<KlineDataPoint>,
    frame: &mut canvas::Frame,
    price_to_y: impl Fn(Price) -> f32,
    interval_to_x: impl Fn(u64) -> f32,
    layout: &FootprintCellLayout<'_>,
    studies: &[FootprintStudy],
    visible_earliest: u64,
    visible_latest: u64,
    imb_study_on: bool,
) {
    let Some(lookback) = studies.iter().find_map(|study| {
        if let FootprintStudy::NPoC { lookback } = study {
            Some(*lookback)
        } else {
            None
        }
    }) else {
        return;
    };

    let (filled_color, naked_color) = (
        layout.pal.background.strong.color,
        if layout.pal.is_dark {
            layout.pal.warning.weak.color.scale_alpha(0.5)
        } else {
            layout.pal.warning.strong.color
        },
    );

    let line_height = layout.cell_h.min(1.0);

    let bar_width_factor: f32 = 0.98;
    let inset = (layout.cell_w * (1.0 - bar_width_factor)) / 2.0;

    let candle_lane_factor: f32 = match layout.cluster {
        ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => 0.25,
        ClusterKind::BidAsk | ClusterKind::Table => 1.0,
    };

    let start_x_for = |cell_center_x: f32| -> f32 {
        match layout.cluster {
            ClusterKind::Table | ClusterKind::BidAsk => {
                cell_center_x + (layout.cell_w / 2.0) - inset
            }
            ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => {
                let content_left = (cell_center_x - (layout.cell_w / 2.0)) + inset;
                let candle_lane_left = content_left
                    + if imb_study_on {
                        layout.candle_w + layout.gaps.marker_to_candle
                    } else {
                        0.0
                    };
                candle_lane_left
                    + layout.candle_w * candle_lane_factor
                    + layout.gaps.candle_to_cluster
            }
        }
    };

    let wick_x_for = |cell_center_x: f32| -> f32 {
        match layout.cluster {
            ClusterKind::BidAsk | ClusterKind::Table => cell_center_x,
            ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => {
                let content_left = (cell_center_x - (layout.cell_w / 2.0)) + inset;
                let candle_lane_left = content_left
                    + if imb_study_on {
                        layout.candle_w + layout.gaps.marker_to_candle
                    } else {
                        0.0
                    };
                candle_lane_left + (layout.candle_w * candle_lane_factor) / 2.0
                    - (layout.gaps.candle_to_cluster * 0.5)
            }
        }
    };

    let end_x_for = |cell_center_x: f32| -> f32 {
        match layout.cluster {
            ClusterKind::Table | ClusterKind::BidAsk => {
                let content_left = cell_center_x - (layout.cell_w / 2.0) + inset;
                let content_right = cell_center_x + (layout.cell_w / 2.0) - inset;
                let table_layout = TableLayout::new(
                    content_left,
                    content_right,
                    layout.candle_w,
                    layout.gaps,
                    imb_study_on,
                );
                table_layout.table_left
            }
            ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => wick_x_for(cell_center_x),
        }
    };

    let rightmost_cell_center_x = {
        let earliest_x = interval_to_x(visible_earliest);
        let latest_x = interval_to_x(visible_latest);
        if earliest_x > latest_x {
            earliest_x
        } else {
            latest_x
        }
    };

    let mut draw_the_line = |interval: u64, poc: &PointOfControl| {
        let start_x = start_x_for(interval_to_x(interval));

        let (line_width, color) = match poc.status {
            NPoc::Naked => {
                let end_x = end_x_for(rightmost_cell_center_x);
                let line_width = end_x - start_x;
                if line_width.abs() <= layout.cell_w {
                    return;
                }
                (line_width, naked_color)
            }
            NPoc::Filled { at } => {
                let end_x = end_x_for(interval_to_x(at));
                let line_width = end_x - start_x;
                if line_width.abs() <= layout.cell_w {
                    return;
                }
                (line_width, filled_color)
            }
            _ => return,
        };

        frame.fill_rectangle(
            Point::new(start_x, price_to_y(poc.price) - line_height / 2.0),
            Size::new(line_width, line_height),
            color,
        );
    };

    match data_source {
        PlotData::TickBased(tick_aggr) => {
            tick_aggr
                .datapoints
                .iter()
                .rev()
                .enumerate()
                .take(lookback)
                .filter_map(|(index, dp)| dp.footprint.poc.as_ref().map(|poc| (index as u64, poc)))
                .for_each(|(interval, poc)| draw_the_line(interval, poc));
        }
        PlotData::TimeBased(timeseries) => {
            timeseries
                .datapoints
                .iter()
                .rev()
                .take(lookback)
                .filter_map(|(timestamp, dp)| {
                    dp.footprint
                        .poc
                        .as_ref()
                        .map(|poc| (timestamp.as_u64(), poc))
                })
                .for_each(|(interval, poc)| draw_the_line(interval, poc));
        }
    }
}

fn cluster_label_size(available_w: f32, cell_h: f32, scaling: f32) -> Option<f32> {
    let scaling = scaling.max(0.1);
    let screen_h = cell_h * scaling;
    let screen_w = available_w.max(0.0) * scaling;
    // Keep values visible while zoomed/panned away — shrink the font instead
    // of dropping it, and only hide labels once they'd be a smudge.
    if screen_h < 4.5 || screen_w < 10.0 {
        return None;
    }
    // ~5-character labels ("12.3k") in a monospace chart font.
    let size = (screen_w / 4.6).min(screen_h * 0.72).min(14.0);
    (size >= 4.5).then_some(size / scaling)
}

fn draw_clusters(
    frame: &mut canvas::Frame,
    price_to_y: impl Fn(Price) -> f32,
    x_position: f32,
    layout: &FootprintCellLayout<'_>,
    scaling: f32,
    max_cluster_qty: f64,
    qty_is_quote: bool,
    step: PriceStep,
    show_summary: bool,
    imbalance: Option<(usize, Option<usize>, bool)>,
    kline: &Kline,
    footprint: &KlineTrades,
    visible_high: Price,
    visible_low: Price,
) {
    let text_color = layout.pal.background.weakest.text;
    let max_cluster_qty = max_cluster_qty.max(1.0);

    let bar_width_factor: f32 = 0.96;
    let inset = (layout.cell_w * (1.0 - bar_width_factor)) / 2.0;

    let cell_left = x_position - (layout.cell_w / 2.0);
    let content_left = cell_left + inset;
    let content_right = x_position + (layout.cell_w / 2.0) - inset;

    let mut table_layout: Option<TableLayout> = None;
    // Retain one adjacent row because cell geometry and diagonal imbalance
    // markers can overlap the viewport edge. Candle summaries and POC remain
    // derived from the complete footprint below.
    let draw_high = visible_high.add_steps(1, step);
    let draw_low = visible_low.add_steps(-1, step);

    match layout.cluster {
        ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => {
            let area = ProfileArea::new(
                content_left,
                content_right,
                layout.candle_w,
                layout.gaps,
                imbalance.is_some(),
            );
            let text_size = cluster_label_size(area.bars_width, layout.cell_h, scaling);
            let bar_alpha = if text_size.is_some() { 0.25 } else { 1.0 };

            for (price, group) in &footprint.trades {
                if *price < draw_low || *price > draw_high {
                    continue;
                }
                let buy_base = group.buy_qty.to_f64();
                let sell_base = group.sell_qty.to_f64();
                let buy_qty = usd_notional(*price, buy_base, qty_is_quote);
                let sell_qty = usd_notional(*price, sell_base, qty_is_quote);
                let y = price_to_y(*price);

                match layout.cluster {
                    ClusterKind::VolumeProfile => {
                        super::draw_volume_bar(
                            frame,
                            area.bars_left,
                            y,
                            buy_qty,
                            sell_qty,
                            max_cluster_qty,
                            area.bars_width,
                            layout.cell_h,
                            layout.pal.success.base.color,
                            layout.pal.danger.base.color,
                            bar_alpha,
                            true,
                        );

                        if let Some(text_size) = text_size {
                            draw_cluster_text(
                                frame,
                                &abbr_large_numbers(buy_qty + sell_qty),
                                Point::new(area.bars_left, y),
                                text_size,
                                text_color,
                                Alignment::Start,
                                Alignment::Center,
                            );
                        }
                    }
                    ClusterKind::DeltaProfile => {
                        let delta = buy_qty - sell_qty;
                        if let Some(text_size) = text_size {
                            draw_cluster_text(
                                frame,
                                &abbr_large_numbers(delta),
                                Point::new(area.bars_left, y),
                                text_size,
                                text_color,
                                Alignment::Start,
                                Alignment::Center,
                            );
                        }

                        let bar_width = ((delta.abs() / max_cluster_qty) as f32 * area.bars_width)
                            .clamp(0.0, area.bars_width);
                        if bar_width > 0.0 {
                            let color = if delta >= 0.0 {
                                layout.pal.success.base.color.scale_alpha(bar_alpha)
                            } else {
                                layout.pal.danger.base.color.scale_alpha(bar_alpha)
                            };
                            frame.fill_rectangle(
                                Point::new(area.bars_left, y - (layout.cell_h / 2.0)),
                                Size::new(bar_width, layout.cell_h),
                                color,
                            );
                        }
                    }
                    _ => {}
                }

                if let Some((threshold, color_scale, ignore_zeros)) = imbalance {
                    let higher_price = price.add_steps(1, step);

                    let rect_w = ((area.imb_marker_width - 1.0) / 2.0).max(1.0);
                    let buyside_x = area.imb_marker_left + area.imb_marker_width - rect_w;
                    let sellside_x =
                        area.imb_marker_left + area.imb_marker_width - (2.0 * rect_w) - 1.0;

                    draw_imbalance_markers(
                        frame,
                        &price_to_y,
                        footprint,
                        *price,
                        sell_base,
                        higher_price,
                        threshold,
                        color_scale,
                        ignore_zeros,
                        layout.cell_h,
                        layout.pal,
                        buyside_x,
                        sellside_x,
                        rect_w,
                    );
                }
            }

            draw_footprint_kline(
                frame,
                &price_to_y,
                area.candle_center_x,
                layout.candle_w,
                kline,
                layout.pal,
            );
        }
        ClusterKind::Table => {
            let tl = TableLayout::new(
                content_left,
                content_right,
                layout.candle_w,
                layout.gaps,
                imbalance.is_some(),
            );
            let area = TableArea::new(frame, &price_to_y, &tl, layout.candle_w, kline, layout.pal);
            table_layout = Some(tl);
            let table_width = area.width();
            let half_width = table_width / 2.0;
            let mid_x = area.table_left + half_width;
            let cell_border = 1.0 / scaling.max(0.1);
            let text_size =
                cluster_label_size(half_width - 4.0 / scaling.max(0.1), layout.cell_h, scaling);
            for (price, group) in &footprint.trades {
                if *price < draw_low || *price > draw_high {
                    continue;
                }
                let buy_base = group.buy_qty.to_f64();
                let sell_base = group.sell_qty.to_f64();
                let buy_qty = usd_notional(*price, buy_base, qty_is_quote);
                let sell_qty = usd_notional(*price, sell_base, qty_is_quote);
                let y = price_to_y(*price);
                let row_top = y - (layout.cell_h / 2.0);

                frame.fill_rectangle(
                    Point::new(area.table_left, row_top),
                    Size::new(half_width, layout.cell_h),
                    ImbalanceSide::Sell.volume_bg_color(sell_qty, max_cluster_qty, layout.pal),
                );
                frame.fill_rectangle(
                    Point::new(area.table_left + half_width, row_top),
                    Size::new(half_width, layout.cell_h),
                    ImbalanceSide::Buy.volume_bg_color(buy_qty, max_cluster_qty, layout.pal),
                );
                let sell_text_color = ImbalanceSide::Sell.volume_text_color(
                    sell_qty,
                    max_cluster_qty,
                    text_color,
                    layout.pal,
                );
                let buy_text_color = ImbalanceSide::Buy.volume_text_color(
                    buy_qty,
                    max_cluster_qty,
                    text_color,
                    layout.pal,
                );

                if let Some((threshold, color_scale, ignore_zeros)) = imbalance {
                    if let Some(alpha) = ImbalanceSide::Sell.color_alpha(
                        footprint,
                        *price,
                        sell_base,
                        step,
                        threshold,
                        color_scale,
                        ignore_zeros,
                    ) {
                        ImbalanceSide::Sell.draw_table_marker(
                            frame,
                            layout.pal,
                            alpha,
                            sell_qty,
                            max_cluster_qty,
                            Rectangle::new(
                                Point::new(area.table_left, row_top),
                                Size::new(half_width, layout.cell_h),
                            ),
                        );
                    }

                    if let Some(alpha) = ImbalanceSide::Buy.color_alpha(
                        footprint,
                        *price,
                        buy_base,
                        step,
                        threshold,
                        color_scale,
                        ignore_zeros,
                    ) {
                        ImbalanceSide::Buy.draw_table_marker(
                            frame,
                            layout.pal,
                            alpha,
                            buy_qty,
                            max_cluster_qty,
                            Rectangle::new(
                                Point::new(mid_x, row_top),
                                Size::new(half_width, layout.cell_h),
                            ),
                        );
                    }
                }

                frame.fill_rectangle(
                    Point::new(area.table_left + half_width, row_top),
                    Size::new(cell_border, layout.cell_h),
                    layout.pal.background.weakest.text.scale_alpha(0.32),
                );

                if footprint
                    .poc
                    .as_ref()
                    .is_some_and(|poc| poc.price == *price)
                {
                    frame.stroke(
                        &Path::rectangle(
                            Point::new(area.table_left, row_top),
                            Size::new(table_width, layout.cell_h),
                        ),
                        Stroke::default()
                            .with_color(layout.pal.primary.strong.color)
                            .with_width(1.0),
                    );
                }

                if let Some(text_size) = text_size {
                    draw_cluster_text(
                        frame,
                        &abbr_large_numbers(sell_qty),
                        Point::new(area.table_left + half_width - 3.0 / scaling.max(0.1), y),
                        text_size,
                        sell_text_color,
                        Alignment::End,
                        Alignment::Center,
                    );
                    draw_cluster_text(
                        frame,
                        &abbr_large_numbers(buy_qty),
                        Point::new(area.table_left + half_width + 3.0 / scaling.max(0.1), y),
                        text_size,
                        buy_text_color,
                        Alignment::Start,
                        Alignment::Center,
                    );
                }
            }
        }
        ClusterKind::BidAsk => {
            let area = BidAskArea::new(content_left, content_right, layout.candle_w, layout.gaps);

            let imb_marker_reserve = if imbalance.is_some() {
                ((area.imb_marker_width - 1.0) / 2.0).max(1.0)
            } else {
                0.0
            };

            let right_max_x =
                area.bid_area_right - imb_marker_reserve - (2.0 * layout.gaps.marker_to_bars);
            let right_area_width = (right_max_x - area.bid_area_left).max(0.0);

            let left_min_x =
                area.ask_area_left + imb_marker_reserve + (2.0 * layout.gaps.marker_to_bars);
            let left_area_width = (area.ask_area_right - left_min_x).max(0.0);
            let text_size = cluster_label_size(
                left_area_width.min(right_area_width),
                layout.cell_h,
                scaling,
            );
            let bar_alpha = if text_size.is_some() { 0.25 } else { 1.0 };

            for (price, group) in &footprint.trades {
                if *price < draw_low || *price > draw_high {
                    continue;
                }
                let buy_base = group.buy_qty.to_f64();
                let sell_base = group.sell_qty.to_f64();
                let buy_qty = usd_notional(*price, buy_base, qty_is_quote);
                let sell_qty = usd_notional(*price, sell_base, qty_is_quote);
                let y = price_to_y(*price);

                if buy_qty > 0.0 && right_area_width > 0.0 {
                    if let Some(text_size) = text_size {
                        draw_cluster_text(
                            frame,
                            &abbr_large_numbers(buy_qty),
                            Point::new(area.bid_area_left, y),
                            text_size,
                            text_color,
                            Alignment::Start,
                            Alignment::Center,
                        );
                    }

                    let bar_width = ((buy_qty / max_cluster_qty) as f32 * right_area_width)
                        .clamp(0.0, right_area_width);
                    if bar_width > 0.0 {
                        frame.fill_rectangle(
                            Point::new(area.bid_area_left, y - (layout.cell_h / 2.0)),
                            Size::new(bar_width, layout.cell_h),
                            layout.pal.success.base.color.scale_alpha(bar_alpha),
                        );
                    }
                }
                if sell_qty > 0.0 && left_area_width > 0.0 {
                    if let Some(text_size) = text_size {
                        draw_cluster_text(
                            frame,
                            &abbr_large_numbers(sell_qty),
                            Point::new(area.ask_area_right, y),
                            text_size,
                            text_color,
                            Alignment::End,
                            Alignment::Center,
                        );
                    }

                    let bar_width = ((sell_qty / max_cluster_qty) as f32 * left_area_width)
                        .clamp(0.0, left_area_width);
                    if bar_width > 0.0 {
                        frame.fill_rectangle(
                            Point::new(area.ask_area_right - bar_width, y - (layout.cell_h / 2.0)),
                            Size::new(bar_width, layout.cell_h),
                            layout.pal.danger.base.color.scale_alpha(bar_alpha),
                        );
                    }
                }

                if let Some((threshold, color_scale, ignore_zeros)) = imbalance
                    && area.imb_marker_width > 0.0
                {
                    let higher_price = price.add_steps(1, step);

                    let rect_width = ((area.imb_marker_width - 1.0) / 2.0).max(1.0);

                    let buyside_x = area.bid_area_right - rect_width - layout.gaps.marker_to_bars;
                    let sellside_x = area.ask_area_left + layout.gaps.marker_to_bars;

                    draw_imbalance_markers(
                        frame,
                        &price_to_y,
                        footprint,
                        *price,
                        sell_base,
                        higher_price,
                        threshold,
                        color_scale,
                        ignore_zeros,
                        layout.cell_h,
                        layout.pal,
                        buyside_x,
                        sellside_x,
                        rect_width,
                    );
                }
            }

            draw_footprint_kline(
                frame,
                &price_to_y,
                area.candle_center_x,
                layout.candle_w,
                kline,
                layout.pal,
            );
        }
    }

    if show_summary {
        let Some((total_notional, delta_notional)) =
            footprint_notional_summary(footprint, qty_is_quote)
        else {
            return;
        };

        let summary_layout = FootprintSummaryLayout::new(layout.cell_h, scaling);

        let summary_x = match layout.cluster {
            ClusterKind::Table => {
                let tl = table_layout
                    .as_ref()
                    .expect("TableLayout must be set for Table cluster");
                (tl.table_left + tl.table_right) / 2.0
            }
            _ => x_position,
        };

        let lowest_trade_price = footprint.trades.keys().min();

        let summary_y = match lowest_trade_price {
            Some(p) => price_to_y(*p) + layout.cell_h / 2.0 + summary_layout.gap,
            None => price_to_y(kline.low) + layout.cell_h / 2.0 + summary_layout.gap,
        };

        draw_cluster_text(
            frame,
            &format!("V: ${}", abbr_large_numbers(total_notional)),
            Point::new(summary_x, summary_y),
            summary_layout.text_size,
            layout.pal.background.weakest.text,
            Alignment::Center,
            Alignment::Start,
        );

        let delta_color = if delta_notional >= 0.0 {
            layout.pal.success.base.color
        } else {
            layout.pal.danger.base.color
        };

        draw_cluster_text(
            frame,
            &format!(
                "Δ: {}${}",
                if delta_notional >= 0.0 { "+" } else { "-" },
                abbr_large_numbers(delta_notional.abs())
            ),
            Point::new(
                summary_x,
                summary_y + summary_layout.text_size + summary_layout.line_gap,
            ),
            summary_layout.text_size,
            delta_color,
            Alignment::Center,
            Alignment::Start,
        );
    }
}

fn usd_notional(price: Price, qty: f64, qty_is_quote: bool) -> f64 {
    if qty_is_quote {
        qty
    } else {
        price.to_f64() * qty
    }
}

fn max_cluster_notional(footprint: &KlineTrades, cluster: ClusterKind, qty_is_quote: bool) -> f64 {
    footprint
        .trades
        .iter()
        .map(|(price, group)| {
            let buy = usd_notional(*price, group.buy_qty.to_f64(), qty_is_quote);
            let sell = usd_notional(*price, group.sell_qty.to_f64(), qty_is_quote);
            match cluster {
                ClusterKind::BidAsk | ClusterKind::Table => buy.max(sell),
                ClusterKind::DeltaProfile => (buy - sell).abs(),
                ClusterKind::VolumeProfile => buy + sell,
            }
        })
        .fold(0.0, f64::max)
}

fn effective_notional_scale(scaling: ClusterScaling, visible_max: f64, individual_max: f64) -> f64 {
    match scaling {
        ClusterScaling::VisibleRange => visible_max.max(1.0),
        ClusterScaling::Datapoint => individual_max.max(1.0),
        ClusterScaling::Hybrid { weight } => {
            let weight = f64::from(weight.clamp(0.0, 1.0));
            (visible_max * weight + individual_max * (1.0 - weight)).max(1.0)
        }
    }
}

fn footprint_notional_summary(footprint: &KlineTrades, qty_is_quote: bool) -> Option<(f64, f64)> {
    (!footprint.trades.is_empty()).then(|| {
        footprint
            .trades
            .iter()
            .fold((0.0, 0.0), |(total, delta), (price, group)| {
                let buy = usd_notional(*price, group.buy_qty.to_f64(), qty_is_quote);
                let sell = usd_notional(*price, group.sell_qty.to_f64(), qty_is_quote);
                (total + buy + sell, delta + buy - sell)
            })
    })
}

fn draw_imbalance_markers(
    frame: &mut canvas::Frame,
    price_to_y: &impl Fn(Price) -> f32,
    footprint: &KlineTrades,
    price: Price,
    sell_qty: f64,
    higher_price: Price,
    threshold: usize,
    color_scale: Option<usize>,
    ignore_zeros: bool,
    cell_height: f32,
    palette: &Extended,
    buyside_x: f32,
    sellside_x: f32,
    rect_width: f32,
) {
    if ignore_zeros && sell_qty <= 0.0 {
        return;
    }

    if let Some(group) = footprint.trades.get(&higher_price) {
        let diagonal_buy_qty = group.buy_qty.to_f64();

        if ignore_zeros && diagonal_buy_qty <= 0.0 {
            return;
        }

        let rect_height = cell_height / 2.0;

        let alpha_from_ratio = |ratio: f64| -> f32 {
            if let Some(scale) = color_scale {
                let divisor = (scale as f64 / 10.0) - 1.0;
                (0.2 + 0.8 * ((ratio - 1.0) / divisor).min(1.0)).min(1.0) as f32
            } else {
                1.0
            }
        };

        if diagonal_buy_qty >= sell_qty {
            let required_qty = sell_qty * (100 + threshold) as f64 / 100.0;
            if diagonal_buy_qty > required_qty {
                let ratio = diagonal_buy_qty / required_qty;
                let alpha = alpha_from_ratio(ratio);

                let y = price_to_y(higher_price);
                frame.fill_rectangle(
                    Point::new(buyside_x, y - (rect_height / 2.0)),
                    Size::new(rect_width, rect_height),
                    ImbalanceSide::Buy.marker_bg_color(palette, alpha),
                );
            }
        } else {
            let required_qty = diagonal_buy_qty * (100 + threshold) as f64 / 100.0;
            if sell_qty > required_qty {
                let ratio = sell_qty / required_qty;
                let alpha = alpha_from_ratio(ratio);

                let y = price_to_y(price);
                frame.fill_rectangle(
                    Point::new(sellside_x, y - (rect_height / 2.0)),
                    Size::new(rect_width, rect_height),
                    ImbalanceSide::Sell.marker_bg_color(palette, alpha),
                );
            }
        }
    }
}

fn draw_cluster_text(
    frame: &mut canvas::Frame,
    text: &str,
    position: Point,
    text_size: f32,
    color: iced::Color,
    align_x: Alignment,
    align_y: Alignment,
) {
    frame.fill_text(canvas::Text {
        content: text.to_string(),
        position,
        size: iced::Pixels(text_size),
        color,
        align_x: align_x.into(),
        align_y: align_y.into(),
        font: style::AZERET_MONO,
        ..canvas::Text::default()
    });
}

fn draw_crosshair_tooltip(
    data: &PlotData<KlineDataPoint>,
    ticker_info: &TickerInfo,
    frame: &mut canvas::Frame,
    palette: &Extended,
    basis: Basis,
    at_interval: Option<u64>,
    visible_range: (u64, u64),
) {
    let (visible_earliest, visible_latest) = visible_range;

    let kline_opt = match (data, at_interval) {
        (PlotData::TimeBased(timeseries), Some(at_interval)) => {
            let in_visible = at_interval >= visible_earliest && at_interval <= visible_latest;

            timeseries
                .datapoints
                .get(&UnixMs::new(at_interval))
                .map(|dp| &dp.kline)
                .or_else(|| {
                    if in_visible {
                        let search_end = at_interval.min(visible_latest);
                        timeseries
                            .datapoints
                            .range(UnixMs::new(visible_earliest)..=UnixMs::new(search_end))
                            .next_back()
                            .map(|(_, dp)| &dp.kline)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    let right_of_latest = match basis {
                        Basis::Time(_) => at_interval > visible_latest,
                        Basis::Tick(_) => at_interval < visible_earliest,
                    };

                    if right_of_latest {
                        timeseries
                            .datapoints
                            .range(UnixMs::new(visible_earliest)..=UnixMs::new(visible_latest))
                            .next_back()
                            .map(|(_, dp)| &dp.kline)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    let (last_time, dp) = timeseries.datapoints.last_key_value()?;
                    (at_interval > last_time.as_u64()).then_some(&dp.kline)
                })
        }
        (PlotData::TickBased(tick_aggr), Some(at_interval)) => {
            let kline_at = |interval: u64| {
                let index = (interval / u64::from(tick_aggr.interval.0)) as usize;
                (index < tick_aggr.datapoints.len())
                    .then(|| &tick_aggr.datapoints[tick_aggr.datapoints.len() - 1 - index].kline)
            };

            let in_visible = at_interval >= visible_earliest && at_interval <= visible_latest;

            kline_at(at_interval).or_else(|| {
                let right_of_latest = match basis {
                    Basis::Time(_) => at_interval > visible_latest,
                    Basis::Tick(_) => at_interval < visible_earliest,
                };

                if in_visible || right_of_latest {
                    kline_at(visible_earliest)
                } else {
                    None
                }
            })
        }
        (PlotData::TimeBased(timeseries), None) => timeseries
            .datapoints
            .last_key_value()
            .map(|(_, dp)| &dp.kline),
        (PlotData::TickBased(tick_aggr), None) => tick_aggr.datapoints.last().map(|dp| &dp.kline),
    };

    if let Some(kline) = kline_opt {
        let change_pct = ((kline.close - kline.open) / kline.open * 100.0) as f32;
        let change_color = if change_pct >= 0.0 {
            palette.success.base.color
        } else {
            palette.danger.base.color
        };

        let base_color = palette.background.base.text;
        let precision = ticker_info.min_ticksize;

        let segments = [
            ("O", base_color, false),
            (&kline.open.to_string(precision), change_color, true),
            ("H", base_color, false),
            (&kline.high.to_string(precision), change_color, true),
            ("L", base_color, false),
            (&kline.low.to_string(precision), change_color, true),
            ("C", base_color, false),
            (&kline.close.to_string(precision), change_color, true),
            (&format!("{change_pct:+.2}%"), change_color, true),
        ];

        let total_width: f32 = segments
            .iter()
            .map(|(s, _, _)| s.len() as f32 * (TEXT_SIZE * 0.8))
            .sum();

        let position = Point::new(8.0, 8.0);

        let tooltip_rect = Rectangle {
            x: position.x,
            y: position.y,
            width: total_width,
            height: 16.0,
        };

        frame.fill_rectangle(
            tooltip_rect.position(),
            tooltip_rect.size(),
            palette.background.weakest.color.scale_alpha(0.9),
        );

        let mut x = position.x;
        for (text, seg_color, is_value) in segments {
            frame.fill_text(canvas::Text {
                content: text.to_string(),
                position: Point::new(x, position.y),
                size: iced::Pixels(crate::style::text_size::BODY),
                color: seg_color,
                font: style::AZERET_MONO,
                ..canvas::Text::default()
            });
            x += text.len() as f32 * 8.0;
            x += if is_value { 6.0 } else { 2.0 };
        }
    }
}

#[derive(Clone, Copy)]
enum ImbalanceSide {
    Buy,
    Sell,
}

impl ImbalanceSide {
    fn volume_bg_color(self, qty: f64, max_qty: f64, palette: &Extended) -> Color {
        // Strong floor so even low-volume rows read as solid cells (the
        // reference footprint look); intensity still ramps to full alpha.
        const MIN_ALPHA: f32 = 0.45;

        let intensity = if max_qty > 0.0 {
            (qty / max_qty).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let alpha = MIN_ALPHA + intensity * (1.0 - MIN_ALPHA);

        match self {
            ImbalanceSide::Buy => palette.success.base.color.scale_alpha(alpha),
            ImbalanceSide::Sell => palette.danger.base.color.scale_alpha(alpha),
        }
    }

    fn volume_text_color(
        self,
        qty: f64,
        max_qty: f64,
        default_color: Color,
        palette: &Extended,
    ) -> Color {
        let cell_color = self.volume_bg_color(qty, max_qty, palette);
        let cell_background = composite_color(cell_color, palette.background.base.color);
        let inverted_color = palette.background.base.color;

        if contrast_ratio(cell_background, inverted_color)
            > contrast_ratio(cell_background, default_color)
        {
            inverted_color
        } else {
            default_color
        }
    }

    fn marker_bg_color(self, palette: &Extended, alpha: f32) -> Color {
        let accent = match self {
            ImbalanceSide::Buy => palette.success.strong.color,
            ImbalanceSide::Sell => palette.danger.strong.color,
        };
        let alpha = alpha.clamp(0.0, 1.0);

        if palette.is_dark {
            let tint = 0.28 + (alpha * 0.32);
            mix_color(accent, palette.background.strongest.color, tint)
        } else {
            let tint = 0.18 + (alpha * 0.24);
            mix_color(accent, palette.background.weak.color, tint)
        }
    }

    fn color_alpha(
        self,
        footprint: &KlineTrades,
        price: Price,
        qty: f64,
        step: PriceStep,
        threshold: usize,
        color_scale: Option<usize>,
        ignore_zeros: bool,
    ) -> Option<f32> {
        let diagonal_price = match self {
            ImbalanceSide::Buy => price.add_steps(-1, step),
            ImbalanceSide::Sell => price.add_steps(1, step),
        };
        let diagonal_qty = footprint
            .trades
            .get(&diagonal_price)
            .map(|group| match self {
                ImbalanceSide::Buy => group.sell_qty.to_f64(),
                ImbalanceSide::Sell => group.buy_qty.to_f64(),
            })
            .unwrap_or_default();

        if ignore_zeros && (qty <= 0.0 || diagonal_qty <= 0.0) {
            return None;
        }

        let required_qty = diagonal_qty * (100 + threshold) as f64 / 100.0;

        if required_qty <= 0.0 {
            return (qty > 0.0).then_some(1.0);
        }

        if qty <= required_qty {
            return None;
        }

        let ratio = qty / required_qty;
        Some(if let Some(scale) = color_scale {
            let divisor = (scale as f64 / 10.0) - 1.0;
            (0.2 + 0.8 * ((ratio - 1.0) / divisor).min(1.0)).min(1.0) as f32
        } else {
            1.0
        })
    }

    fn draw_table_marker(
        self,
        frame: &mut canvas::Frame,
        palette: &Extended,
        alpha: f32,
        qty: f64,
        max_qty: f64,
        cell: Rectangle,
    ) {
        if cell.height <= 0.0 {
            return;
        }

        let bar_width = 2.5;
        let gap = 1.5;

        let volume_intensity = if max_qty > 0.0 {
            (qty / max_qty).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let imbalance_strength = alpha.clamp(0.0, 1.0);
        let marker_alpha = 0.38 + (volume_intensity * 0.24) + (imbalance_strength * 0.38);
        let marker_alpha = marker_alpha.clamp(0.38, 1.0);

        let color = palette.warning.strong.color.scale_alpha(marker_alpha);

        let (x, bar_w) = match self {
            ImbalanceSide::Sell => (cell.x - bar_width - gap, bar_width),
            ImbalanceSide::Buy => (cell.x + cell.width + gap, bar_width),
        };

        frame.fill_rectangle(Point::new(x, cell.y), Size::new(bar_w, cell.height), color);
    }
}

#[derive(Clone, Copy, Debug)]
struct ContentGaps {
    /// Space between imb. markers candle body
    marker_to_candle: f32,
    /// Space between candle body and clusters
    candle_to_cluster: f32,
    /// Inner space reserved between imb. markers and clusters (used for BidAsk)
    marker_to_bars: f32,
    /// Gutter between the sell and buy halves in BidAsk mode
    bid_to_ask: f32,
}

impl ContentGaps {
    fn from_view(candle_width: f32, scaling: f32) -> Self {
        let px = |p: f32| p / scaling.max(0.1);
        let base = (candle_width * 0.2).max(px(2.0));
        Self {
            marker_to_candle: base,
            candle_to_cluster: base,
            marker_to_bars: px(2.0),
            bid_to_ask: px(8.0),
        }
    }
}

/// Layout and style parameters shared across footprint cell draw functions.
struct FootprintCellLayout<'a> {
    cell_w: f32,
    cell_h: f32,
    candle_w: f32,
    pal: &'a Extended,
    cluster: ClusterKind,
    gaps: ContentGaps,
}

struct ProfileArea {
    imb_marker_left: f32,
    imb_marker_width: f32,
    bars_left: f32,
    bars_width: f32,
    candle_center_x: f32,
}

impl ProfileArea {
    fn new(
        content_left: f32,
        content_right: f32,
        candle_width: f32,
        gaps: ContentGaps,
        has_imbalance: bool,
    ) -> Self {
        let candle_lane_left = if has_imbalance {
            content_left + candle_width + gaps.marker_to_candle
        } else {
            content_left
        };
        let candle_lane_width = candle_width * 0.25;

        let bars_left = candle_lane_left + candle_lane_width + gaps.candle_to_cluster;
        let bars_width = (content_right - bars_left).max(0.0);

        let candle_center_x = candle_lane_left + (candle_lane_width / 2.0);

        Self {
            imb_marker_left: content_left,
            imb_marker_width: if has_imbalance { candle_width } else { 0.0 },
            bars_left,
            bars_width,
            candle_center_x,
        }
    }
}

struct BidAskArea {
    bid_area_left: f32,
    bid_area_right: f32,
    ask_area_left: f32,
    ask_area_right: f32,
    candle_center_x: f32,
    imb_marker_width: f32,
}

impl BidAskArea {
    fn new(content_left: f32, content_right: f32, candle_width: f32, spacing: ContentGaps) -> Self {
        // Candle lives in its own lane on the left edge of the cell, both
        // histograms sit to its right (reference footprint layout).
        let candle_center_x = content_left + candle_width / 2.0;
        let hist_left =
            (content_left + candle_width + spacing.candle_to_cluster).min(content_right);
        let half_width = ((content_right - hist_left) / 2.0).max(0.0);

        // Keep a readable gutter between the sell and buy halves.
        let mid = hist_left + half_width;
        let gutter = spacing.bid_to_ask.min(half_width);
        let ask_area_left = hist_left;
        let ask_area_right = (mid - gutter / 2.0).max(hist_left);
        let bid_area_left = (mid + gutter / 2.0).min(content_right);
        let bid_area_right = content_right;

        Self {
            bid_area_left,
            bid_area_right,
            ask_area_left,
            ask_area_right,
            candle_center_x,
            imb_marker_width: candle_width,
        }
    }
}

struct TableLayout {
    table_left: f32,
    table_right: f32,
    candle_center_x: f32,
}

impl TableLayout {
    fn new(
        content_left: f32,
        content_right: f32,
        candle_width: f32,
        spacing: ContentGaps,
        has_imbalance: bool,
    ) -> Self {
        let (candle_center_x, table_left) = if has_imbalance {
            let ccx = content_left + candle_width / 2.0;
            let tl = (content_left + candle_width + spacing.candle_to_cluster).min(content_right);
            (ccx, tl)
        } else {
            let thin_candle = candle_width * 0.25;
            let ccx = content_left + thin_candle / 2.0;
            let tl = (content_left + thin_candle + spacing.candle_to_cluster).min(content_right);
            (ccx, tl)
        };

        Self {
            table_left,
            table_right: content_right,
            candle_center_x,
        }
    }
}

struct TableArea {
    table_left: f32,
    table_right: f32,
}

impl TableArea {
    fn new(
        frame: &mut canvas::Frame,
        price_to_y: &impl Fn(Price) -> f32,
        table_layout: &TableLayout,
        candle_width: f32,
        kline: &Kline,
        palette: &Extended,
    ) -> Self {
        draw_footprint_kline(
            frame,
            price_to_y,
            table_layout.candle_center_x,
            candle_width,
            kline,
            palette,
        );

        Self {
            table_left: table_layout.table_left,
            table_right: table_layout.table_right,
        }
    }

    fn width(&self) -> f32 {
        (self.table_right - self.table_left).max(0.0)
    }
}

struct FootprintSummaryLayout {
    text_size: f32,
    gap: f32,
    line_gap: f32,
}

impl FootprintSummaryLayout {
    /// Computes the text size, gap, and line gap for footprint summary text.
    /// Scales the font down when the on-screen cell height is too small.
    fn new(cell_height: f32, scaling: f32) -> FootprintSummaryLayout {
        const MIN_SCREEN_CELL_H_PX: f32 = 6.0;
        const MIN_TEXT_SIZE_PX: f32 = 3.0;
        const SUMMARY_GAP_PX: f32 = 8.0;
        const SUMMARY_LINE_GAP_PX: f32 = 2.0;

        let max_text_size = style::text_size::TINY;
        let screen_cell_h = cell_height * scaling;
        let text_size = if screen_cell_h < MIN_SCREEN_CELL_H_PX {
            (max_text_size * (screen_cell_h / MIN_SCREEN_CELL_H_PX)).max(MIN_TEXT_SIZE_PX)
        } else {
            max_text_size
        };

        let gap = SUMMARY_GAP_PX / scaling;
        let line_gap = SUMMARY_LINE_GAP_PX / scaling;

        FootprintSummaryLayout {
            text_size,
            gap,
            line_gap,
        }
    }

    fn padding(cell_height: f32, scaling: f32, tick_size: f32) -> f32 {
        if cell_height <= f32::EPSILON {
            return 0.0;
        }

        let layout = Self::new(cell_height, scaling);

        let first_line_bottom = layout.gap + layout.text_size;
        let second_line_bottom = first_line_bottom + layout.line_gap + layout.text_size;

        let summary_ticks = second_line_bottom / cell_height;
        summary_ticks * tick_size
    }
}

#[inline]
fn price_padding_from_pixels(cell_height: f32, tick_size: f32) -> f32 {
    const OUTER_BOUND_PADDING_PX: f32 = 4.0;

    if cell_height <= f32::EPSILON {
        return 0.0;
    }

    (OUTER_BOUND_PADDING_PX / cell_height) * tick_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, Trade, Volume, adapter::Exchange, unit::Qty};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Mutex;

    static TRADE_FETCH_MODE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct CacheClearProbe {
        clear_count: Rc<Cell<usize>>,
        insert_count: Rc<Cell<usize>>,
    }

    impl KlineIndicatorImpl for CacheClearProbe {
        fn clear_all_caches(&mut self) {
            self.clear_count.set(self.clear_count.get() + 1);
        }

        fn clear_crosshair_caches(&mut self) {}

        fn element<'a>(
            &'a self,
            _chart: &'a ViewState,
            _data_labels_always_visible: bool,
            _visible_range: std::ops::RangeInclusive<u64>,
        ) -> Element<'a, Message> {
            iced::widget::row![].into()
        }

        fn on_insert_trades(
            &mut self,
            trades: &[Trade],
            _old_dp_len: usize,
            _source: &PlotData<KlineDataPoint>,
        ) {
            self.insert_count
                .set(self.insert_count.get() + trades.len());
        }
    }

    fn test_trade(price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(1_000),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    fn tpo_seed_bar(day: u64, low: f64, high: f64) -> Kline {
        let open = Price::from_f64((low + high) * 0.5);
        Kline {
            time: UnixMs::new(day * 86_400_000 + 60_000),
            open,
            high: Price::from_f64(high),
            low: Price::from_f64(low),
            close: open,
            volume: Volume::TotalOnly(Qty::ZERO),
        }
    }

    fn tpo_single_print_fixture(config: TpoConfig, days: u64) -> (PriceStep, TickAggr) {
        let step = PriceStep {
            units: Price::from_f64(1.0).units,
        };
        let mut bars = Vec::new();
        for day in 0..days {
            let day_start = day * 86_400_000;
            let shift = (day % 20) as f64;
            for (block, low, high) in [(0_u64, 100.0, 500.0), (1, 100.0, 220.0), (2, 280.0, 500.0)]
            {
                let open = Price::from_f64(low + shift);
                bars.push(Kline {
                    time: UnixMs::new(day_start + block * 30 * 60_000),
                    open,
                    high: Price::from_f64(high + shift),
                    low: Price::from_f64(low + shift),
                    close: Price::from_f64(high + shift),
                    volume: Volume::TotalOnly(Qty::ZERO),
                });
            }
        }

        (step, TickAggr::new_tpo_seeded(config, step, &bars, &[]))
    }

    #[test]
    fn tpo_single_print_extension_stops_at_first_future_tag() {
        let config = TpoConfig {
            ticks_per_row: 1,
            ..TpoConfig::default()
        };
        let bars = [
            tpo_seed_bar(1, 100.0, 102.0),
            tpo_seed_bar(2, 101.0, 102.0),
            tpo_seed_bar(3, 100.0, 101.0),
        ];
        let tick_aggr = TickAggr::new_tpo_seeded(
            config,
            PriceStep {
                units: Price::from_f64(1.0).units,
            },
            &bars,
            &[],
        );

        // Rendering offsets run newest-to-oldest: 0=newest, 2=oldest.
        assert_eq!(
            first_future_tpo_tag_offset(&tick_aggr.datapoints, 2, Price::from_f64(101.0)),
            Some(1),
            "the immediately following profile is the first tag"
        );
        assert_eq!(
            first_future_tpo_tag_offset(&tick_aggr.datapoints, 2, Price::from_f64(100.0)),
            Some(0),
            "the extension must skip a non-tagging profile"
        );
        assert_eq!(
            first_future_tpo_tag_offset(&tick_aggr.datapoints, 1, Price::from_f64(102.0)),
            None,
            "older profiles must never count as future tags"
        );
        assert_eq!(
            first_future_tpo_tag_offset(&tick_aggr.datapoints, 0, Price::from_f64(101.0)),
            None,
            "the newest profile has no future tag"
        );
    }

    #[test]
    fn prepared_tpo_extensions_match_reference_lookup() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: 8,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) = tpo_single_print_fixture(config, 8);
        let prepared = prepare_tpo_render(
            &tick_aggr.datapoints,
            0,
            tick_aggr.datapoints.len() - 1,
            config,
            44.0,
            1.0,
            step,
            Price::from_f64(1_000.0),
            Price::from_f64(0.0),
            None,
        );

        let mut extension_count = 0usize;
        for prepared_profile in prepared {
            let actual = prepared_profile
                .extensions
                .iter()
                .map(|extension| (extension.price, extension.tag.map(|tag| tag.offset)))
                .collect::<Vec<_>>();
            let expected = prepared_profile
                .profile
                .single_print_prices()
                .map(|price| {
                    (
                        price,
                        first_future_tpo_tag_offset(
                            &tick_aggr.datapoints,
                            prepared_profile.offset,
                            price,
                        ),
                    )
                })
                .collect::<Vec<_>>();
            extension_count += actual.len();
            assert_eq!(actual, expected, "offset={}", prepared_profile.offset);
        }
        assert!(extension_count > 0);
    }

    #[test]
    fn prepared_tpo_extensions_are_culled_to_visible_rows_with_padding() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: 4,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) = tpo_single_print_fixture(config, 4);
        let prepared = prepare_tpo_render(
            &tick_aggr.datapoints,
            0,
            tick_aggr.datapoints.len() - 1,
            config,
            44.0,
            1.0,
            step,
            Price::from_f64(241.0),
            Price::from_f64(240.0),
            None,
        );
        let padded_low = Price::from_units(Price::from_f64(240.0).units - step.units);
        let padded_high = Price::from_units(Price::from_f64(241.0).units + step.units);
        let extension_count = prepared
            .iter()
            .flat_map(|profile| &profile.extensions)
            .inspect(|extension| {
                assert!(extension.price >= padded_low);
                assert!(extension.price <= padded_high);
            })
            .count();

        assert!(extension_count > 0);
        assert!(extension_count < 4 * 59);
    }

    #[test]
    fn structural_composite_render_plan_dims_only_completed_member_profiles() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: 4,
            show_three_day_composite: true,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) = tpo_single_print_fixture(config, 4);
        let now = UnixMs::new(3 * TpoProfilePeriod::Day.millis() + 1);
        let composite = structural_composite_for_render(&tick_aggr.datapoints, config, now)
            .expect("three completed days form the fixture balance");
        assert_eq!(composite.profile_count, 3);

        let prepared = prepare_tpo_render(
            &tick_aggr.datapoints,
            0,
            tick_aggr.datapoints.len() - 1,
            config,
            44.0,
            1.0,
            step,
            Price::from_f64(1_000.0),
            Price::from_f64(0.0),
            Some(&composite),
        );
        let member_profiles = prepared
            .iter()
            .filter(|profile| profile.opacity < 1.0)
            .collect::<Vec<_>>();
        assert_eq!(member_profiles.len(), 3);
        assert!(member_profiles.iter().all(|profile| {
            profile.extensions.is_empty()
                && composite.contains_profile(profile.profile)
                && (profile.opacity - 0.20).abs() < f32::EPSILON
        }));
        assert_eq!(
            prepared
                .iter()
                .filter(|profile| profile.opacity == 1.0)
                .count(),
            1
        );
    }

    #[test]
    fn developing_tpo_updates_reuse_the_structural_composite_cache() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: 4,
            show_three_day_composite: true,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) = tpo_single_print_fixture(config, 4);
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            1.0,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Tick(data::aggr::TickCount(1)),
            step,
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Tpo { config },
            None,
        );
        chart.data_source = PlotData::TickBased(tick_aggr);
        chart.refresh_tpo_structural_composite(UnixMs::new(3 * TpoProfilePeriod::Day.millis() + 1));
        let cached = std::ptr::from_ref(
            chart
                .tpo_structural_composite
                .as_deref()
                .expect("three completed days form the fixture balance"),
        );

        let mut developing_trade = test_trade(150.0, 0.01, false);
        developing_trade.time = UnixMs::new(3 * TpoProfilePeriod::Day.millis() + 60_000);
        chart.insert_trades(source, &[developing_trade]);

        assert_eq!(
            cached,
            std::ptr::from_ref(
                chart
                    .tpo_structural_composite
                    .as_deref()
                    .expect("developing trade must not discard the completed-session cache")
            )
        );
    }

    /// Manual release-mode benchmark for the work performed when a TPO canvas
    /// cache is rebuilt after zooming or panning. Kept ignored so ordinary test
    /// runs stay deterministic and fast.
    #[test]
    #[ignore = "manual TPO render-preparation benchmark"]
    fn benchmark_tpo_single_print_extension_lookup() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: TpoConfig::MAX_HISTORY_PROFILES,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) =
            tpo_single_print_fixture(config, u64::from(TpoConfig::MAX_HISTORY_PROFILES));
        assert_eq!(
            tick_aggr.datapoints.len(),
            usize::from(TpoConfig::MAX_HISTORY_PROFILES)
        );

        let mut samples = Vec::new();
        let mut final_checksum = 0usize;
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let prepared = prepare_tpo_render(
                &tick_aggr.datapoints,
                0,
                tick_aggr.datapoints.len() - 1,
                config,
                44.0,
                1.0,
                step,
                Price::from_f64(1_000_000.0),
                Price::from_f64(-1_000_000.0),
                None,
            );
            let checksum = prepared
                .iter()
                .flat_map(|profile| &profile.extensions)
                .fold(0usize, |checksum, extension| {
                    checksum.wrapping_add(
                        extension
                            .tag
                            .map_or(usize::MAX, |future_tag| future_tag.offset),
                    )
                });
            std::hint::black_box(&prepared);
            std::hint::black_box(checksum);
            final_checksum = checksum;
            samples.push(started.elapsed());
        }
        assert_eq!(
            final_checksum, 1_677_960,
            "render-preparation truth changed"
        );
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        println!(
            "TPO render preparation optimized: profiles={} rows={} median_ms={:.3} checksum={final_checksum}",
            tick_aggr.datapoints.len(),
            tick_aggr
                .datapoints
                .iter()
                .map(|datapoint| datapoint
                    .tpo
                    .as_ref()
                    .map_or(0, |profile| profile.rows.len()))
                .sum::<usize>(),
            median.as_secs_f64() * 1_000.0,
        );
    }

    #[test]
    #[ignore = "manual structural-composite render benchmark"]
    fn benchmark_tpo_structural_composite_render_plan() {
        let config = TpoConfig {
            ticks_per_row: 1,
            profiles_to_load: TpoConfig::MAX_HISTORY_PROFILES,
            show_three_day_composite: true,
            ..TpoConfig::default()
        };
        let (step, tick_aggr) =
            tpo_single_print_fixture(config, u64::from(TpoConfig::MAX_HISTORY_PROFILES));
        let now = UnixMs::new(
            (u64::from(TpoConfig::MAX_HISTORY_PROFILES) + 1) * TpoProfilePeriod::Day.millis(),
        );
        let derive_started = std::time::Instant::now();
        let composite = structural_composite_for_render(&tick_aggr.datapoints, config, now)
            .expect("benchmark balance");
        let derive_elapsed = derive_started.elapsed();
        let mut samples = Vec::new();
        let mut checksum = 0usize;
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let prepared = prepare_tpo_render(
                &tick_aggr.datapoints,
                0,
                tick_aggr.datapoints.len() - 1,
                config,
                44.0,
                1.0,
                step,
                Price::from_f64(1_000_000.0),
                Price::from_f64(-1_000_000.0),
                Some(&composite),
            );
            checksum = composite
                .rows
                .values()
                .fold(prepared.len(), |sum, count| sum.wrapping_add(*count));
            std::hint::black_box((&composite, &prepared));
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        println!(
            "TPO structural composite cached: profiles={} rows={} derive_once_ms={:.3} render_median_ms={:.3} checksum={checksum}",
            tick_aggr.datapoints.len(),
            tick_aggr
                .datapoints
                .iter()
                .map(|datapoint| datapoint
                    .tpo
                    .as_ref()
                    .map_or(0, |profile| profile.rows.len()))
                .sum::<usize>(),
            derive_elapsed.as_secs_f64() * 1_000.0,
            median.as_secs_f64() * 1_000.0,
        );
    }

    #[test]
    fn tpo_history_boundary_aligns_to_the_profile_session() {
        let config = TpoConfig {
            session_start_minutes_utc: 8 * 60 + 30,
            ..TpoConfig::default()
        };
        let now = UnixMs::new(200 * 86_400_000 + 17 * 60 * 60 * 1_000 + 12_345);
        let boundary = tpo_history_earliest(config, now);

        assert_eq!(boundary, config.profile_start(boundary));
        assert!(boundary <= now.saturating_sub(config.history_range_ms()));
        assert!(
            now.saturating_sub(config.history_range_ms())
                .saturating_diff(boundary)
                < config.profile_period.millis()
        );
    }

    #[test]
    fn incremental_aggregate_tpo_pages_match_a_full_rebuild_for_any_source_order() {
        let primary = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            1.0,
            0.001,
            None,
        );
        let secondary = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            1.0,
            0.001,
            None,
        );
        let feed = ResolvedFeed::aggregated(
            data::aggregation::AggregateFeedId::BtcUsdtPerpetual,
            primary,
            &[primary, secondary],
        );
        let config = TpoConfig {
            ticks_per_row: 1,
            ..TpoConfig::default()
        };
        let step = PriceStep {
            units: Price::from_f64(1.0).units,
        };
        let primary_first = [tpo_seed_bar(1, 100.0, 102.0), tpo_seed_bar(2, 101.0, 103.0)];
        let primary_second = [tpo_seed_bar(3, 102.0, 104.0)];
        let secondary_page = [
            tpo_seed_bar(1, 99.0, 103.0),
            tpo_seed_bar(2, 100.0, 104.0),
            tpo_seed_bar(3, 101.0, 105.0),
        ];

        for pages in [
            [
                (primary, primary_first.as_slice()),
                (primary, primary_second.as_slice()),
                (secondary, secondary_page.as_slice()),
            ],
            [
                (secondary, secondary_page.as_slice()),
                (primary, primary_second.as_slice()),
                (primary, primary_first.as_slice()),
            ],
        ] {
            let mut aggregator = KlineAggregator::new(&feed);
            let mut incremental = TickAggr::new_tpo_seeded(config, step, &[], &[]);
            for (source, page) in pages {
                aggregator.insert(source, page);
                let affected = page
                    .iter()
                    .map(|bar| config.profile_start(bar.time))
                    .collect::<Vec<_>>();
                let start = *affected.iter().min().unwrap();
                let end = affected
                    .iter()
                    .max()
                    .unwrap()
                    .saturating_add(config.profile_period.millis())
                    .saturating_sub(1);
                incremental.replace_tpo_sessions_from_canonical(
                    &affected,
                    &aggregator.composite_klines_in_range(start, end),
                    &[],
                );
            }

            let rebuilt =
                TickAggr::new_tpo_seeded(config, step, &aggregator.composite_klines(), &[]);
            assert_eq!(incremental.datapoints.len(), rebuilt.datapoints.len());
            for (actual, expected) in incremental.datapoints.iter().zip(&rebuilt.datapoints) {
                assert_eq!(actual.kline.open, expected.kline.open);
                assert_eq!(actual.kline.high, expected.kline.high);
                assert_eq!(actual.kline.low, expected.kline.low);
                assert_eq!(actual.kline.close, expected.kline.close);
                let actual = actual.tpo.as_ref().unwrap();
                let expected = expected.tpo.as_ref().unwrap();
                assert_eq!(actual.start, expected.start);
                assert_eq!(actual.end, expected.end);
                assert_eq!(actual.brackets.len(), expected.brackets.len());
                for (block, actual_range) in &actual.brackets {
                    let expected_range = &expected.brackets[block];
                    assert_eq!(actual_range.low, expected_range.low);
                    assert_eq!(actual_range.high, expected_range.high);
                }
                assert_eq!(actual.poc, expected.poc);
                assert_eq!(actual.value_area_high, expected.value_area_high);
                assert_eq!(actual.value_area_low, expected.value_area_low);
                assert_eq!(actual.initial_balance_high, expected.initial_balance_high);
                assert_eq!(actual.initial_balance_low, expected.initial_balance_low);
                assert_eq!(actual.total_tpos, expected.total_tpos);
                assert_eq!(actual.first_time, expected.first_time);
                assert_eq!(actual.last_time, expected.last_time);
                assert_eq!(actual.open, expected.open);
                assert_eq!(actual.close, expected.close);
                assert_eq!(
                    actual
                        .rows
                        .iter()
                        .map(|(price, row)| (*price, row.blocks.clone()))
                        .collect::<Vec<_>>(),
                    expected
                        .rows
                        .iter()
                        .map(|(price, row)| (*price, row.blocks.clone()))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn previous_value_area_uses_its_configured_venue_sources() {
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M15),
            PriceStep::from(binance.min_ticksize),
            &[],
            Vec::new(),
            &[KlineIndicator::PreviousValueArea],
            binance,
            &KlineChartKind::Candles,
            None,
        );

        chart.configure_footprint_history(vec![binance, bybit], true);
        assert_eq!(chart.pva_klines.sources(), &[binance, bybit]);

        chart.configure_footprint_history(vec![bybit, binance], false);
        assert_eq!(chart.pva_klines.sources(), &[bybit]);
    }

    #[test]
    fn aggregated_live_trades_create_footprint_buckets_before_klines() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M5),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        chart.set_feed(ResolvedFeed::direct(source));
        assert!(chart.is_empty());
        chart.invalidate(None);

        let mut trade = test_trade(67_000.0, 0.01, false);
        trade.time = UnixMs::now();
        chart.insert_trades(source, &[trade]);

        assert!(!chart.is_empty());
        assert!(chart.chart.latest_x > 0);
        assert_eq!(chart.raw_trades.len(), 1);
        assert_eq!(chart.raw_trades[0].time, trade.time);
        assert_eq!(chart.raw_trades[0].price, trade.price);
        assert_eq!(chart.raw_trades[0].qty, trade.qty);
        assert_eq!(chart.raw_trades[0].is_sell, trade.is_sell);
        assert!(chart.has_pending_live_redraw());
        let _ = chart.maintain(Instant::now() + LIVE_REDRAW_INTERVAL + Duration::from_millis(1));
        assert!(!chart.has_pending_live_redraw());
        assert_eq!(chart.raw_trades.len(), 1);
    }

    #[test]
    fn live_publication_keeps_unchanged_indicator_geometry_cached() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M5),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        chart.set_feed(ResolvedFeed::direct(source));

        let clear_count = Rc::new(Cell::new(0));
        let insert_count = Rc::new(Cell::new(0));
        chart.indicators[KlineIndicator::OpenInterest] = Some(Box::new(CacheClearProbe {
            clear_count: Rc::clone(&clear_count),
            insert_count: Rc::clone(&insert_count),
        }));
        chart.last_live_redraw = Instant::now() - LIVE_REDRAW_INTERVAL;

        let mut trade = test_trade(67_000.0, 0.01, false);
        trade.time = UnixMs::now();
        chart.insert_trades(source, &[trade]);

        assert_eq!(
            insert_count.get(),
            1,
            "the market event must still be applied"
        );
        assert_eq!(chart.raw_trades.len(), 1);
        assert_eq!(chart.raw_trades[0].time, trade.time);
        assert_eq!(chart.raw_trades[0].price, trade.price);
        assert_eq!(chart.raw_trades[0].qty, trade.qty);
        assert_eq!(chart.raw_trades[0].is_sell, trade.is_sell);
        assert_eq!(
            clear_count.get(),
            0,
            "live publication must retain an unchanged indicator canvas"
        );

        chart.invalidate(None);
        assert_eq!(
            clear_count.get(),
            1,
            "full viewport/config invalidation still clears every indicator"
        );
    }

    #[test]
    fn live_render_coalescing_caps_publication_at_twenty_hz() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M5),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Candles,
            None,
        );
        let started = Instant::now();
        chart.last_live_redraw = started;

        let mut publications = 0usize;
        for elapsed_ms in (10..=1_000).step_by(10) {
            let previous = chart.last_live_redraw;
            chart.invalidate_live_render_at(started + Duration::from_millis(elapsed_ms));
            publications += usize::from(chart.last_live_redraw != previous);
        }

        assert_eq!(publications, 20);
        assert!(!chart.has_pending_live_redraw());
        assert_eq!(
            chart.last_live_redraw,
            started + Duration::from_millis(1_000)
        );
    }

    #[test]
    fn streamed_kline_truth_updates_before_its_deferred_render() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M5),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Candles,
            None,
        );
        chart.last_live_redraw = Instant::now();
        let kline = seed_kline(UnixMs::new(1_800_000_000_000), 67_123.5);

        chart.update_latest_kline(&kline);

        let PlotData::TimeBased(series) = &chart.data_source else {
            panic!("candlestick chart must remain time based");
        };
        let stored = &series.datapoints[&kline.time].kline;
        assert_eq!(stored.time, kline.time);
        assert_eq!(stored.open, kline.open);
        assert_eq!(stored.high, kline.high);
        assert_eq!(stored.low, kline.low);
        assert_eq!(stored.close, kline.close);
        assert!(chart.has_pending_live_redraw());
    }

    #[test]
    fn live_footprint_backfills_each_sources_missing_candle_prefix_once() {
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let feed = ResolvedFeed::from_sources_selected(binance, &[binance, bybit], None);
        let interval = Timeframe::M30;
        let bucket = UnixMs::new(1_800_000_000_000).floor_to(interval);
        let kline = seed_kline(bucket, 67_000.0);
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(interval),
            feed.price_step(),
            &[kline],
            Vec::new(),
            &[],
            binance,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        chart.set_feed(feed);

        let binance_start = bucket.saturating_add(13 * 60_000);
        let bybit_start = bucket.saturating_add(14 * 60_000);
        let mut binance_trade = test_trade(67_000.0, 0.5, false);
        binance_trade.time = binance_start;
        let mut bybit_trade = test_trade(67_000.0, 0.25, true);
        bybit_trade.time = bybit_start;
        chart.insert_trades(binance, &[binance_trade]);
        chart.insert_trades(bybit, &[bybit_trade]);

        let specs = chart.visible_trade_prefix_fetches(
            bucket,
            bucket.saturating_add(interval.to_milliseconds()),
            interval,
            TradeFetchMode::Server,
        );
        assert_eq!(specs.len(), 2);
        for spec in &specs {
            let source = spec.stream.expect("prefix fetch must identify its source");
            let expected_end = if source.ticker_info() == binance {
                binance_start.saturating_sub(1)
            } else if source.ticker_info() == bybit {
                bybit_start.saturating_sub(1)
            } else {
                panic!("unexpected prefix source: {source:?}");
            };
            assert_eq!(
                spec.fetch,
                FetchRange::FootprintTrades(bucket, expected_end)
            );
            chart.request_handler.mark_completed(spec.req_id);
        }

        assert!(
            chart
                .visible_trade_prefix_fetches(
                    bucket,
                    bucket.saturating_add(interval.to_milliseconds()),
                    interval,
                    TradeFetchMode::Server,
                )
                .is_empty(),
            "completed source prefixes must not be requested again"
        );
    }

    #[test]
    fn aggregate_trade_fetch_stays_active_until_every_venue_finishes() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        let range = FetchRange::FootprintTrades(UnixMs::new(1), UnixMs::new(2));
        let specs = [
            Exchange::BinanceLinear,
            Exchange::BybitLinear,
            Exchange::HyperliquidLinear,
        ]
        .into_iter()
        .map(|exchange| FetchSpec {
            req_id: uuid::Uuid::new_v4(),
            fetch: range,
            stream: Some(StreamKind::Trades {
                ticker_info: TickerInfo::new(
                    Ticker::new(
                        if exchange == Exchange::HyperliquidLinear {
                            "BTC"
                        } else {
                            "BTCUSDT"
                        },
                        exchange,
                    ),
                    0.1,
                    0.001,
                    None,
                ),
            }),
        })
        .collect::<Vec<_>>();

        chart.begin_trade_fetches(&specs);
        assert!(chart.is_fetching_trades());
        assert_eq!(chart.active_trade_fetches.len(), 3);

        chart.finalize_trade_fetch(specs[0].req_id);
        assert!(chart.is_fetching_trades());
        assert_eq!(chart.active_trade_fetches.len(), 2);
        chart.finalize_trade_fetch(specs[1].req_id);
        assert!(chart.is_fetching_trades());
        assert_eq!(chart.active_trade_fetches.len(), 1);
        chart.finalize_trade_fetch(specs[2].req_id);
        assert!(!chart.is_fetching_trades());
        assert!(chart.active_trade_fetches.is_empty());
    }

    #[test]
    fn aggregate_chart_rejects_stale_and_deselected_source_pages() {
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let feed =
            ResolvedFeed::from_sources_selected(bybit, &[binance, bybit], Some(&[bybit.ticker]));
        let bucket = UnixMs::new(1_800_000_000_000).floor_to(Timeframe::M30);
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            feed.price_step(),
            &[seed_kline(bucket, 67_000.0)],
            Vec::new(),
            &[],
            bybit,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        chart.set_feed(feed);
        let current_id = uuid::Uuid::new_v4();
        chart.begin_trade_fetches(&[FetchSpec {
            req_id: current_id,
            fetch: FetchRange::FootprintTrades(bucket, bucket.saturating_add(60_000)),
            stream: Some(StreamKind::Trades { ticker_info: bybit }),
        }]);
        let mut trade = test_trade(67_000.0, 1.0, false);
        trade.time = bucket.saturating_add(1_000);

        chart.insert_raw_trades(bybit, vec![trade], false, Some(uuid::Uuid::new_v4()));
        chart.insert_raw_trades(binance, vec![trade], false, Some(current_id));
        let PlotData::TimeBased(series) = &chart.data_source else {
            panic!("time chart expected");
        };
        assert!(series.datapoints[&bucket].footprint.trades.is_empty());

        chart.insert_raw_trades(bybit, vec![trade], false, Some(current_id));
        let PlotData::TimeBased(series) = &chart.data_source else {
            panic!("time chart expected");
        };
        assert_eq!(series.datapoints[&bucket].footprint.trades.len(), 1);
    }

    #[test]
    fn footprint_rows_and_summary_use_usd_notional() {
        let mut footprint = KlineTrades::new();
        let step = PriceStep::from(exchange::unit::MinTicksize::new(0));
        footprint.add_trade_to_nearest_bin(&test_trade(100.0, 2.0, false), step);
        footprint.add_trade_to_nearest_bin(&test_trade(100.0, 1.0, true), step);

        assert_eq!(
            max_cluster_notional(&footprint, ClusterKind::Table, false),
            200.0
        );
        assert_eq!(
            footprint_notional_summary(&footprint, false),
            Some((300.0, 100.0))
        );
        assert_eq!(
            footprint_notional_summary(&footprint, true),
            Some((3.0, 1.0)),
            "quote-normalized quantities must not be multiplied by price twice"
        );
    }

    #[test]
    fn vpvr_grouping_uses_feed_ticks_not_footprint_display_rows() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let config = data::chart::kline::Config {
            vpvr_ticks: 10,
            ..Default::default()
        };
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            &[KlineIndicator::VisibleRangeProfile],
            source,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            Some(config),
        );
        chart.change_tick_size(PriceStep {
            units: Price::from_f64(50.0).units,
        });

        assert_eq!(chart.chart.tick_size.to_ui_string(), "50");
        assert_eq!(chart.vpvr_group_step().to_ui_string(), "1");
    }

    #[test]
    fn cluster_labels_hide_when_the_cell_is_too_small() {
        assert!(cluster_label_size(40.0, 16.0, 1.0).is_some());
        assert!(cluster_label_size(10.0, 16.0, 1.0).is_none());
        assert!(cluster_label_size(80.0, 4.0, 1.0).is_none());
    }

    #[test]
    fn kline_history_pages_do_not_double_fill_existing_footprints() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let step = PriceStep::from(source.min_ticksize);
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M5),
            step,
            &[],
            Vec::new(),
            &[],
            source,
            &KlineChartKind::Footprint {
                clusters: ClusterKind::Table,
                scaling: ClusterScaling::default(),
                studies: vec![],
            },
            None,
        );
        chart.set_feed(ResolvedFeed::direct(source));

        let bucket = UnixMs::now().floor_to(Timeframe::M5);
        let mut trade = test_trade(67_000.0, 0.5, false);
        trade.time = bucket;
        chart.insert_trades(source, &[trade]);

        let qty_before = match &chart.data_source {
            PlotData::TimeBased(series) => series
                .datapoints
                .values()
                .next()
                .unwrap()
                .footprint
                .trades
                .values()
                .next()
                .unwrap()
                .buy_qty
                .to_f64(),
            PlotData::TickBased(_) => panic!("expected time series"),
        };

        let kline = Kline {
            time: bucket,
            open: Price::from_f64(67_000.0),
            high: Price::from_f64(67_000.0),
            low: Price::from_f64(67_000.0),
            close: Price::from_f64(67_000.0),
            volume: Volume::empty_buy_sell(),
        };
        chart.insert_hist_klines(uuid::Uuid::new_v4(), source, &[kline]);

        let qty_after = match &chart.data_source {
            PlotData::TimeBased(series) => series
                .datapoints
                .values()
                .next()
                .unwrap()
                .footprint
                .trades
                .values()
                .next()
                .unwrap()
                .buy_qty
                .to_f64(),
            PlotData::TickBased(_) => panic!("expected time series"),
        };
        assert_eq!(qty_before, 0.5);
        assert_eq!(qty_after, 0.5);
    }

    #[test]
    fn hyperliquid_only_daily_delta_uses_shared_btc_tick() {
        let hyperliquid = TickerInfo::new(
            Ticker::new("BTC", Exchange::HyperliquidLinear),
            1.0,
            0.001,
            None,
        );
        let fallback: PriceStep = exchange::unit::MinTicksize::new(0).into();

        let step = shared_history_price_step(&[hyperliquid], fallback);

        assert_eq!(step.to_ui_string(), "0.1");
        assert_eq!(
            TickMultiplier(1500).multiply_step(step).to_ui_string(),
            "150"
        );
    }

    #[test]
    fn empty_hyperliquid_day_does_not_block_the_next_history_day() {
        let _guard = TRADE_FETCH_MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Server);
        let source = TickerInfo::new(
            Ticker::new("CACHE_MISS_TEST", Exchange::HyperliquidLinear),
            1.0,
            0.001,
            None,
        );
        let price = Price::from_f64(68_000.0);
        let kline = Kline {
            time: UnixMs::now(),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::TotalOnly(Qty::ZERO),
        };
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            PriceStep::from(source.min_ticksize),
            &[kline],
            Vec::new(),
            &[KlineIndicator::DailyDelta],
            source,
            &KlineChartKind::Candles,
            None,
        );
        chart.configure_footprint_history(vec![source], true);

        let Action::RequestFetch(first) = chart.fetch_footprint_history().unwrap() else {
            panic!("first history day should be requested");
        };
        let first = first.into_iter().next().unwrap();
        let FetchRange::FootprintHistoryTrades(first_start, _) = first.fetch else {
            panic!("daily delta should request UTC-day trades");
        };
        chart.mark_fetch_no_data(first.req_id);

        let Action::RequestFetch(next) = chart.fetch_footprint_history().unwrap() else {
            panic!("the next history day should be requested after an empty day");
        };
        let FetchRange::FootprintHistoryTrades(next_start, _) = next[0].fetch else {
            panic!("daily delta should continue with UTC-day trades");
        };
        assert!(next_start < first_start);
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Off);
    }

    #[test]
    fn cumulative_delta_backfill_is_bounded_to_its_fetch_horizon() {
        let _guard = TRADE_FETCH_MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Server);
        let source = TickerInfo::new(
            Ticker::new("CACHE_MISS_TEST", Exchange::HyperliquidLinear),
            1.0,
            0.001,
            None,
        );
        let price = Price::from_f64(68_000.0);
        let kline = Kline {
            time: UnixMs::now(),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::TotalOnly(Qty::ZERO),
        };
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            PriceStep::from(source.min_ticksize),
            &[kline],
            Vec::new(),
            &[KlineIndicator::CumulativeDelta],
            source,
            &KlineChartKind::Candles,
            None,
        );
        chart.configure_footprint_history(vec![source], true);

        // Drain the whole backfill, treating every day as empty. Planning must
        // stop after the indicator's fetch horizon — not its multi-month
        // retention window, which would flood rate-limited REST sources and
        // starve the chart's own kline fetches.
        let horizon =
            u64::from(crate::chart::indicator::kline::cumulative_delta::FETCH_LOOKBACK_DAYS);
        let cutoff = chart.footprint_history.cutoff.unwrap_or_else(UnixMs::now);
        let oldest_allowed = crate::chart::indicator::kline::footprint_history::day_start(cutoff)
            .saturating_sub(
                (horizon.saturating_sub(1))
                    * crate::chart::indicator::kline::footprint_history::DAY_MS,
            );

        let mut requested_days = Vec::new();
        loop {
            // The trade-fetch mode is process-global and sibling tests flip it;
            // re-assert it so planning is not cut short mid-drain.
            crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Server);
            let Some(Action::RequestFetch(specs)) = chart.fetch_footprint_history() else {
                break;
            };
            for spec in specs {
                let FetchRange::FootprintHistoryTrades(start, _) = spec.fetch else {
                    panic!("cumulative delta should only request UTC-day trades");
                };
                requested_days.push(start);
                chart.mark_fetch_no_data(spec.req_id);
            }
        }

        assert_eq!(requested_days.len(), horizon as usize);
        assert!(
            requested_days
                .iter()
                .all(|day| day.as_u64() >= oldest_allowed),
            "requested {requested_days:?} extends past the {oldest_allowed} fetch horizon"
        );
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Off);
    }

    #[test]
    fn enabling_trade_history_indicator_releases_aborted_venue_requests() {
        let _guard = TRADE_FETCH_MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Server);
        let binance = TickerInfo::new(
            Ticker::new("CACHE_MISS_BINANCE", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("CACHE_MISS_BYBIT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let price = Price::from_f64(68_000.0);
        let kline = Kline {
            time: UnixMs::now(),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::TotalOnly(Qty::ZERO),
        };
        let mut chart = KlineChart::new(
            ViewConfig::default(),
            Basis::Time(Timeframe::M30),
            PriceStep::from(binance.min_ticksize),
            &[kline],
            Vec::new(),
            &[KlineIndicator::CumulativeDelta],
            binance,
            &KlineChartKind::Candles,
            None,
        );
        chart.configure_footprint_history(vec![binance, bybit], true);

        let Action::RequestFetch(initial) = chart
            .fetch_footprint_history()
            .expect("both venue backfills should start")
        else {
            panic!("history fetch should return requests");
        };
        assert_eq!(initial.len(), 2);
        assert_eq!(chart.footprint_history.trade_requests.len(), 2);

        chart.toggle_indicator(KlineIndicator::LargeTrades);
        assert!(chart.footprint_history.trade_requests.is_empty());

        let Action::RequestFetch(retried) = chart
            .fetch_footprint_history()
            .expect("aborted venue backfills must be retried")
        else {
            panic!("history retry should return requests");
        };
        assert_eq!(retried.len(), 2);
        assert!(retried.iter().any(|spec| {
            spec.stream
                .is_some_and(|stream| stream.ticker_info() == binance)
        }));
        assert!(retried.iter().any(|spec| {
            spec.stream
                .is_some_and(|stream| stream.ticker_info() == bybit)
        }));
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Off);
    }

    fn seed_kline(time: UnixMs, close: f64) -> Kline {
        let price = Price::from_f64(close);
        Kline {
            time,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::TotalOnly(Qty::ZERO),
        }
    }

    fn renko_chart(source: TickerInfo, indicators: &[KlineIndicator]) -> KlineChart {
        KlineChart::new(
            ViewConfig::default(),
            Basis::Tick(data::aggr::TickCount(1)),
            PriceStep::from(source.min_ticksize),
            &[],
            Vec::new(),
            indicators,
            source,
            &KlineChartKind::Renko {
                config: RenkoConfig::default(),
            },
            None,
        )
    }

    #[test]
    fn renko_seed_pages_backward_after_the_first_minute_page() {
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = renko_chart(source, &[]);

        let Action::RequestFetch(first) = chart
            .fetch_missing_data()
            .expect("first 1m seed page should be requested")
        else {
            panic!("renko seed should request 1m klines");
        };
        let FetchRange::Kline(first_start, first_end) = first[0].fetch else {
            panic!("renko seed should request klines, got {:?}", first[0].fetch);
        };
        assert!(
            first_end.as_u64() - first_start.as_u64() >= 1_000 * 60_000,
            "first page should cover a venue-sized 1m window, got {first_start:?}..{first_end:?}"
        );

        let minute_ms = RENKO_SEED_TIMEFRAME.to_milliseconds();
        let page_bars = ((first_end.as_u64() - first_start.as_u64()) / minute_ms) as usize;
        let klines: Vec<Kline> = (0..page_bars)
            .map(|i| seed_kline(first_start.saturating_add(i as u64 * minute_ms), 68_000.0))
            .collect();
        chart.insert_hist_klines(first[0].req_id, source, &klines);

        let Action::RequestFetch(second) = chart
            .fetch_missing_data()
            .expect("next 1m seed page must be dispatchable after insert")
        else {
            panic!("renko seed should keep requesting 1m klines");
        };
        let FetchRange::Kline(second_start, second_end) = second[0].fetch else {
            panic!(
                "renko seed should request klines, got {:?}",
                second[0].fetch
            );
        };
        assert!(
            second_end < first_start,
            "second page {second_start:?}..{second_end:?} should be older than first {first_start:?}..{first_end:?}"
        );
        assert!(chart.needs_seed_backfill());
    }

    #[test]
    fn renko_seed_pages_are_not_starved_by_cumulative_delta_backfill() {
        let _guard = TRADE_FETCH_MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Server);
        let source = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut chart = renko_chart(source, &[KlineIndicator::CumulativeDelta]);
        chart.configure_footprint_history(vec![source], true);

        let Action::RequestFetch(specs) = chart
            .fetch_missing_data()
            .expect("renko seed should still be requested with CVD enabled")
        else {
            panic!("renko seed should request 1m klines");
        };
        assert!(
            matches!(specs[0].fetch, FetchRange::Kline(..)),
            "chart seed must win over CVD trade backfill, got {:?}",
            specs[0].fetch
        );
        crate::connector::fetcher::set_trade_fetch_mode(TradeFetchMode::Off);
    }
}
