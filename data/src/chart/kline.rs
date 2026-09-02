use crate::{aggr::time::DataPoint, chart::indicator::KlineIndicator};
use exchange::{
    Kline, TickMultiplier, Trade, UnixMs,
    unit::price::{Price, PriceStep},
    unit::qty::Qty,
};

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Smallest on-screen height of one TPO price row when zoomed fully out.
const TPO_MIN_ROW_HEIGHT_PX: f32 = 1.5;

#[derive(Clone)]
pub struct KlineDataPoint {
    pub kline: Kline,
    pub footprint: KlineTrades,
}

impl KlineDataPoint {
    pub fn max_cluster_qty(&self, cluster_kind: ClusterKind, highest: Price, lowest: Price) -> Qty {
        self.footprint
            .max_cluster_qty(cluster_kind, highest, lowest)
    }

    pub fn add_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.footprint.add_trade_to_nearest_bin(trade, step);
    }

    pub fn poc_price(&self) -> Option<Price> {
        self.footprint.poc_price()
    }

    pub fn set_poc_status(&mut self, status: NPoc) {
        self.footprint.set_poc_status(status);
    }

    pub fn clear_trades(&mut self) {
        self.footprint.clear();
    }

    pub fn calculate_poc(&mut self) {
        self.footprint.calculate_poc();
    }

    pub fn last_trade_time(&self) -> Option<UnixMs> {
        self.footprint.last_trade_t()
    }

    pub fn first_trade_time(&self) -> Option<UnixMs> {
        self.footprint.first_trade_t()
    }

    pub fn volume_delta(&self) -> Qty {
        if self.kline.volume.is_directional() {
            self.kline.volume.delta()
        } else if !self.footprint.trades.is_empty() {
            self.footprint
                .trades
                .values()
                .fold(Qty::ZERO, |acc, group| acc + group.delta_qty())
        } else {
            Qty::ZERO
        }
    }

    /// Whether this datapoint has directional (buy vs sell) data.
    pub fn is_directional(&self) -> bool {
        !self.footprint.trades.is_empty() || self.kline.volume.is_directional()
    }
}

impl DataPoint for KlineDataPoint {
    fn add_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.add_trade(trade, step);
    }

    fn clear_trades(&mut self) {
        self.clear_trades();
    }

    fn last_trade_time(&self) -> Option<UnixMs> {
        self.last_trade_time()
    }

    fn first_trade_time(&self) -> Option<UnixMs> {
        self.first_trade_time()
    }

    fn last_price(&self) -> Price {
        self.kline.close
    }

    fn kline(&self) -> Option<&Kline> {
        Some(&self.kline)
    }

    fn value_high(&self) -> Price {
        self.kline.high
    }

    fn value_low(&self) -> Price {
        self.kline.low
    }
}

#[derive(Debug, Clone, Default)]
pub struct GroupedTrades {
    pub buy_qty: Qty,
    pub sell_qty: Qty,
    pub first_time: UnixMs,
    pub last_time: UnixMs,
    pub buy_count: usize,
    pub sell_count: usize,
}

impl GroupedTrades {
    fn new(trade: &Trade) -> Self {
        Self {
            buy_qty: if trade.is_sell {
                Qty::default()
            } else {
                trade.qty
            },
            sell_qty: if trade.is_sell {
                trade.qty
            } else {
                Qty::default()
            },
            first_time: trade.time,
            last_time: trade.time,
            buy_count: if trade.is_sell { 0 } else { 1 },
            sell_count: if trade.is_sell { 1 } else { 0 },
        }
    }

    fn add_trade(&mut self, trade: &Trade) {
        if trade.is_sell {
            self.sell_qty += trade.qty;
            self.sell_count += 1;
        } else {
            self.buy_qty += trade.qty;
            self.buy_count += 1;
        }
        self.first_time = self.first_time.min(trade.time);
        self.last_time = self.last_time.max(trade.time);
    }

    fn merge(&mut self, other: &Self) {
        self.buy_qty += other.buy_qty;
        self.sell_qty += other.sell_qty;
        self.buy_count += other.buy_count;
        self.sell_count += other.sell_count;
        if other.first_time < self.first_time {
            self.first_time = other.first_time;
        }
        if other.last_time > self.last_time {
            self.last_time = other.last_time;
        }
    }

    pub fn total_qty(&self) -> Qty {
        self.buy_qty + self.sell_qty
    }

    pub fn delta_qty(&self) -> Qty {
        self.buy_qty - self.sell_qty
    }

    pub fn max_cluster_qty(&self, cluster_kind: ClusterKind) -> Qty {
        match cluster_kind {
            ClusterKind::BidAsk | ClusterKind::Table => self.buy_qty.max(self.sell_qty),
            ClusterKind::DeltaProfile => self.buy_qty.abs_diff(self.sell_qty),
            ClusterKind::VolumeProfile => self.total_qty(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct KlineTrades {
    pub trades: FxHashMap<Price, GroupedTrades>,
    pub poc: Option<PointOfControl>,
}

impl KlineTrades {
    pub fn new() -> Self {
        Self {
            trades: FxHashMap::default(),
            poc: None,
        }
    }

    pub fn first_trade_t(&self) -> Option<UnixMs> {
        self.trades.values().map(|group| group.first_time).min()
    }

    pub fn last_trade_t(&self) -> Option<UnixMs> {
        self.trades.values().map(|group| group.last_time).max()
    }

    /// Add trade to the bin at the step multiple computed with side-based rounding.
    /// Intended for order-book ladder/quotes; Floor for sells, ceil for buys.
    /// Introduces side bias at bin edges and should not be used for OHLC/footprint aggregation
    pub fn add_trade_to_side_bin(&mut self, trade: &Trade, step: PriceStep) {
        let price = trade.price.round_to_side_step(trade.is_sell, step);

        self.trades
            .entry(price)
            .and_modify(|group| group.add_trade(trade))
            .or_insert_with(|| GroupedTrades::new(trade));
    }

    /// Add trade to the bin at the nearest step multiple (side-agnostic).
    /// Ties (exactly half a step) round up to the higher multiple.
    /// Intended for footprint/OHLC trade aggregation
    pub fn add_trade_to_nearest_bin(&mut self, trade: &Trade, step: PriceStep) {
        let price = trade.price.round_to_step(step);

        self.trades
            .entry(price)
            .and_modify(|group| group.add_trade(trade))
            .or_insert_with(|| GroupedTrades::new(trade));
    }

    /// Merge already-binned levels onto a coarser price step without needing
    /// the original trade list. Tick-size changes must do this or the
    /// histogram keeps drawing the old 0.1 grid.
    pub fn rebin(&mut self, step: PriceStep) {
        *self = self.grouped_to_step(step);
    }

    /// One footprint row per `step` multiple. Drawing must use this, not the
    /// raw map: live/history inserts can land on a finer grid than the
    /// selected tick multiplier, and those rows paint on top of each other.
    pub fn grouped_to_step(&self, step: PriceStep) -> Self {
        if self.is_grouped_to_step(step) {
            return self.clone();
        }

        let mut grouped = Self::new();
        for (price, group) in &self.trades {
            let key = price.round_to_step(step);
            grouped
                .trades
                .entry(key)
                .and_modify(|existing| existing.merge(group))
                .or_insert_with(|| group.clone());
        }
        grouped.calculate_poc();
        if let (Some(src), Some(dst)) = (self.poc, grouped.poc.as_mut()) {
            dst.status = src.status;
        }
        grouped
    }

    /// Render-oriented grouping view. Tick-based footprints and multiplier-1
    /// time footprints are already stored on their display grid, so borrow
    /// those maps instead of cloning every visible price level on pan/zoom.
    pub fn grouped_to_step_cow(&self, step: PriceStep) -> Cow<'_, Self> {
        if self.is_grouped_to_step(step) {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(self.grouped_to_step(step))
        }
    }

    fn is_grouped_to_step(&self, step: PriceStep) -> bool {
        step.units <= 1
            || self.trades.is_empty()
            || self
                .trades
                .keys()
                .all(|price| price.units.rem_euclid(step.units) == 0)
    }

    /// Max of some extracted qty across price levels within [`lowest`, `highest`].
    pub fn max_qty_by<F>(&self, highest: Price, lowest: Price, f: F) -> Qty
    where
        F: Fn(&GroupedTrades) -> Qty,
    {
        let mut max_qty = Qty::default();
        for (price, group) in &self.trades {
            if *price >= lowest && *price <= highest {
                max_qty = max_qty.max(f(group));
            }
        }
        max_qty
    }

    /// Max cluster qty considering only price levels within [`lowest`, `highest`].
    /// Used by the aggregate visible-range computation where the viewport may
    /// show only a subset of each bar's full price range (y-pan/zoom).
    pub fn max_cluster_qty(&self, cluster_kind: ClusterKind, highest: Price, lowest: Price) -> Qty {
        self.max_qty_by(highest, lowest, |group| group.max_cluster_qty(cluster_kind))
    }

    /// Max cluster qty across all price levels in this bar (unfiltered).
    /// Used by per-bar individual scaling, the full bar should contribute.
    pub fn max_cluster_qty_all(&self, cluster_kind: ClusterKind) -> Qty {
        self.trades
            .values()
            .map(|group| group.max_cluster_qty(cluster_kind))
            .max()
            .unwrap_or_default()
    }

    /// Max cluster qty as it would appear when rows are merged onto
    /// `group_step` (the same merge [`Self::grouped_to_step`] performs at
    /// draw time), without allocating the merged histogram.
    ///
    /// Storage may sit on a finer grid than the selected tick multiplier;
    /// scaling must be computed against the *displayed* row quantities or
    /// cells render over-saturated.
    pub fn max_cluster_qty_grouped(
        &self,
        cluster_kind: ClusterKind,
        highest: Price,
        lowest: Price,
        group_step: PriceStep,
    ) -> Qty {
        if self.is_grouped_to_step(group_step) {
            return self.max_qty_by(highest, lowest, |group| group.max_cluster_qty(cluster_kind));
        }

        let mut buckets: FxHashMap<Price, Qty> = FxHashMap::default();
        for (price, group) in &self.trades {
            if *price < lowest || *price > highest {
                continue;
            }
            let key = price.round_to_step(group_step);
            buckets
                .entry(key)
                .and_modify(|qty| *qty += group.max_cluster_qty(cluster_kind))
                .or_insert_with(|| group.max_cluster_qty(cluster_kind));
        }
        buckets.values().copied().max().unwrap_or_default()
    }

    pub fn calculate_poc(&mut self) {
        if self.trades.is_empty() {
            return;
        }

        let mut max_volume = Qty::ZERO;
        let mut poc_price = Price::from_f32(0.0);

        for (price, group) in &self.trades {
            let total_volume = group.total_qty();
            if total_volume > max_volume {
                max_volume = total_volume;
                poc_price = *price;
            }
        }

        self.poc = Some(PointOfControl {
            price: poc_price,
            volume: max_volume,
            status: NPoc::default(),
        });
    }

    pub fn set_poc_status(&mut self, status: NPoc) {
        if let Some(poc) = &mut self.poc {
            poc.status = status;
        }
    }

    pub fn poc_price(&self) -> Option<Price> {
        self.poc.map(|poc| poc.price)
    }

    pub fn clear(&mut self) {
        self.trades.clear();
        self.poc = None;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FootprintSummary {
    pub buy: Qty,
    pub sell: Qty,
    pub total: Qty,
    pub delta: Qty,
    pub delta_pct: f64,
}

impl FootprintSummary {
    pub fn new(buy: Qty, sell: Qty) -> Self {
        let total = buy + sell;
        let delta = buy - sell;
        let total_f = total.to_f64();
        let delta_pct = if total_f > 0.0 {
            (delta.to_f64() / total_f) * 100.0
        } else {
            0.0
        };

        Self {
            buy,
            sell,
            total,
            delta,
            delta_pct,
        }
    }

    pub fn from_trades(footprint: &KlineTrades) -> Option<Self> {
        if footprint.trades.is_empty() {
            return None;
        }

        let (buy, sell) = footprint
            .trades
            .values()
            .fold((Qty::ZERO, Qty::ZERO), |(buy, sell), group| {
                (buy + group.buy_qty, sell + group.sell_qty)
            });

        Some(Self::new(buy, sell))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum KlineChartKind {
    #[default]
    Candles,
    Renko {
        #[serde(default)]
        config: RenkoConfig,
    },
    Tpo {
        #[serde(default)]
        config: super::tpo::Config,
    },
    Footprint {
        clusters: ClusterKind,
        #[serde(default)]
        scaling: ClusterScaling,
        studies: Vec<FootprintStudy>,
    },
}

impl KlineChartKind {
    pub fn allows_indicator(&self, indicator: KlineIndicator) -> bool {
        // CME session edges require the chart's timestamp-keyed OHLC source.
        // Renko/TPO are tick-indexed projections and cannot truthfully recover
        // the exact Friday close and Sunday reopen from their synthetic bars.
        if indicator == KlineIndicator::CmeGap {
            return matches!(
                self,
                KlineChartKind::Candles | KlineChartKind::Footprint { .. }
            );
        }
        if matches!(
            indicator,
            KlineIndicator::DailyDelta
                | KlineIndicator::PreviousValueArea
                | KlineIndicator::LiquidityHeatmap
                | KlineIndicator::VisibleRangeProfile
        ) {
            if indicator == KlineIndicator::LiquidityHeatmap {
                return matches!(
                    self,
                    KlineChartKind::Candles | KlineChartKind::Footprint { .. }
                );
            }
            return matches!(
                self,
                KlineChartKind::Candles
                    | KlineChartKind::Renko { .. }
                    | KlineChartKind::Footprint { .. }
            );
        }
        if indicator == KlineIndicator::FootprintHistory {
            return true;
        }
        !matches!(
            (self, indicator),
            (
                KlineChartKind::Candles | KlineChartKind::Renko { .. },
                KlineIndicator::BarAnalysis
            ) | (KlineChartKind::Tpo { .. }, _)
        )
    }

    pub fn min_scaling(&self) -> f32 {
        match self {
            // Footprint must stay readable when many bars are on screen;
            // labels/candles clamp to minimum screen sizes instead.
            KlineChartKind::Footprint { .. } => 0.1,
            KlineChartKind::Tpo { .. } => 0.4,
            KlineChartKind::Candles | KlineChartKind::Renko { .. } => 0.6,
        }
    }

    pub fn max_scaling(&self) -> f32 {
        match self {
            // TPO needs deeper overall zoom so letter cells can reach readable px.
            KlineChartKind::Tpo { .. } => 2.5,
            KlineChartKind::Footprint { .. } => 16.0,
            KlineChartKind::Candles | KlineChartKind::Renko { .. } => 2.5,
        }
    }

    pub fn max_cell_width(&self) -> f32 {
        match self {
            KlineChartKind::Footprint { .. } => 1_440.0,
            // Allow a single day profile to fill most of the pane when zoomed in.
            KlineChartKind::Tpo { .. } => 520.0,
            KlineChartKind::Candles => 16.0,
            KlineChartKind::Renko { .. } => 24.0,
        }
    }

    pub fn min_cell_width(&self) -> f32 {
        match self {
            // Compact columns so a useful number of footprints fits on
            // screen; text hides itself below readable sizes instead.
            KlineChartKind::Footprint { .. } => 24.0,
            KlineChartKind::Tpo { .. } => 18.0,
            KlineChartKind::Candles => 1.0,
            KlineChartKind::Renko { .. } => 3.0,
        }
    }

    pub fn max_cell_height(&self) -> f32 {
        match self {
            KlineChartKind::Footprint { .. } => 360.0,
            // Per-tick height; visual TPO row = cell_height * ticks_per_row.
            // Higher cap so Y zoom can produce readable letter rows.
            KlineChartKind::Tpo { .. } => 28.0,
            KlineChartKind::Candles | KlineChartKind::Renko { .. } => 8.0,
        }
    }

    pub fn min_cell_height(&self) -> f32 {
        match self {
            KlineChartKind::Footprint { .. } => 1.0,
            // Per-tick height; visual TPO row = cell_height * ticks_per_row.
            // Bound the *visual* row (~1.5 px at scale 1) instead of the raw
            // per-tick height, or large ticks-per-row values make further
            // zoom-out impossible.
            KlineChartKind::Tpo { config } => {
                (TPO_MIN_ROW_HEIGHT_PX / config.ticks_per_row.max(1) as f32).max(0.000_5)
            }
            KlineChartKind::Candles | KlineChartKind::Renko { .. } => 0.001,
        }
    }

    pub fn default_cell_width(&self) -> f32 {
        match self {
            KlineChartKind::Footprint { clusters, .. } => clusters.min_footprint_width(),
            // Give each daily profile a wide time slot so neighboring Market
            // Profiles remain visually distinct, as on reference TPO charts.
            KlineChartKind::Tpo { config }
                if config.profile_period == super::tpo::ProfilePeriod::Day =>
            {
                240.0
            }
            KlineChartKind::Tpo { .. } => 108.0,
            KlineChartKind::Candles => 4.0,
            KlineChartKind::Renko { .. } => 12.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct RenkoConfig {
    /// Size of one brick in exchange minimum-price ticks.
    pub brick_size: u32,
    /// Number of bricks price must travel before a reversal is confirmed.
    pub reversal: u8,
    /// Minimum lifetime of a developing brick. Zero disables rate limiting.
    pub normalization_ms: u32,
}

impl RenkoConfig {
    pub const MIN_BRICK_SIZE: u32 = 1;
    pub const MAX_BRICK_SIZE: u32 = 10_000;
    pub const MIN_REVERSAL: u8 = 1;
    pub const MAX_REVERSAL: u8 = 5;
    pub const BRICK_SIZE_PRESETS: [u32; 14] = [
        5, 10, 20, 50, 100, 150, 200, 300, 350, 500, 1_000, 2_000, 5_000, 10_000,
    ];
    pub const NORMALIZATION_PRESETS_MS: [u32; 8] = [0, 100, 250, 500, 1_000, 2_000, 5_000, 10_000];

    pub fn normalized(self) -> Self {
        Self {
            brick_size: self
                .brick_size
                .clamp(Self::MIN_BRICK_SIZE, Self::MAX_BRICK_SIZE),
            reversal: self.reversal.clamp(Self::MIN_REVERSAL, Self::MAX_REVERSAL),
            normalization_ms: self.normalization_ms.min(10_000),
        }
    }
}

impl Default for RenkoConfig {
    fn default() -> Self {
        // Match common liquid-perp defaults (e.g. 350-tick brick, 2-box reversal).
        Self {
            brick_size: 350,
            reversal: 2,
            normalization_ms: 0,
        }
    }
}

impl std::fmt::Display for RenkoConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Renko {}×{}", self.brick_size, self.reversal)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum ClusterKind {
    BidAsk,
    VolumeProfile,
    DeltaProfile,
    #[default]
    Table,
}

impl ClusterKind {
    pub const ALL: [ClusterKind; 4] = [
        ClusterKind::BidAsk,
        ClusterKind::VolumeProfile,
        ClusterKind::DeltaProfile,
        ClusterKind::Table,
    ];

    /// Minimum footprint cell width (in unscaled pixels) for the cluster rendering mode.
    pub fn min_footprint_width(self) -> f32 {
        match self {
            ClusterKind::VolumeProfile | ClusterKind::DeltaProfile => 80.0,
            // Bid x Ask adds a per-row delta value after the two histogram
            // halves. Give each lane enough room for slightly larger labels.
            ClusterKind::BidAsk => 180.0,
            ClusterKind::Table => 100.0,
        }
    }
}

impl std::fmt::Display for ClusterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClusterKind::BidAsk => write!(f, "Bid × Ask"),
            ClusterKind::VolumeProfile => write!(f, "Volume Profile"),
            ClusterKind::DeltaProfile => write!(f, "Delta Profile"),
            ClusterKind::Table => write!(f, "Table"),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    // Whether to show last value labels on top right/left when not hovering
    // e.g. OHLC/bar change values for the main chart, or last value of an indicator series
    pub data_labels_always_visible: bool,
    // Whether to draw OHLC candle bodies and wicks on footprint charts.
    pub show_footprint_candles: bool,
    // Whether to show the footprint per-bar summary below each candle.
    pub show_footprint_summary: bool,
    /// Highlight summary volume and absolute delta at this multiple of their
    /// trailing per-candle averages.
    pub footprint_summary_abnormal_multiplier: f32,
    // Whether Renko candles show the traded high/low beyond their fixed body.
    pub show_renko_wicks: bool,
    /// Daily Delta grouping in exchange min-ticks. `1` is one price level per min tick.
    pub daily_delta_ticks: u16,
    /// How many UTC days of Daily Delta history to keep and fetch, including today.
    pub daily_delta_days: u16,
    /// Previous-period value-area row grouping in exchange min-ticks.
    ///
    /// Previous Value Areas builds its profiles with the same TPO machinery
    /// (`data::chart::tpo`), so this is the TPO "ticks per row" knob.
    pub previous_value_area_ticks: u16,
    /// Previous Value Areas letter/block size (TPO block size).
    pub previous_value_area_block_size: super::tpo::BlockSize,
    /// UTC minute-of-day anchoring every Previous Value Areas calendar period.
    pub previous_value_area_session_start_minutes_utc: u16,
    /// Previous Value Areas value-area coverage percent.
    pub previous_value_area_value_area_percent: u8,
    /// Minimum displayed liquidity order size in quote currency (USD for USDT/USDC markets).
    pub liquidity_heatmap_order_size_filter: f32,
    /// Minimum executed-trade notional in quote currency shown by the
    /// Large Trades overlay.
    pub large_trades_min_usd: f32,
    /// VPVR grouping in exchange min-ticks. `1` is one price level per min tick.
    pub vpvr_ticks: u16,
}

impl Config {
    pub const FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_MIN: f32 = 1.5;
    pub const FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_MAX: f32 = 10.0;
    pub const FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_DEFAULT: f32 = 3.0;
    pub const FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_STEP: f32 = 0.25;

    pub const DAILY_DELTA_DAY_PRESETS: [u16; 7] = [1, 2, 3, 4, 5, 7, 14];

    /// Large Trades capture floor. Every trade at or above this notional is
    /// retained while it is in the retention window, so lowering the visible
    /// threshold never needs a re-backfill.
    pub const LARGE_TRADES_MIN_USD_MIN: f32 = 100_000.0;
    pub const LARGE_TRADES_MIN_USD_MAX: f32 = 40_000_000.0;
    pub const LARGE_TRADES_MIN_USD_DEFAULT: f32 = 1_000_000.0;
    pub const LARGE_TRADES_MIN_USD_STEP: f32 = 100_000.0;
    pub const LARGE_TRADES_MIN_USD_PRESETS: [f32; 8] = [
        100_000.0,
        250_000.0,
        500_000.0,
        750_000.0,
        1_000_000.0,
        3_000_000.0,
        5_000_000.0,
        10_000_000.0,
    ];

    /// TPO config used to build Previous Value Areas profiles.
    ///
    /// Reuses the exact market-profile machinery behind the TPO chart so both
    /// surfaces agree on VAH/VAL/POC when the knobs match.
    pub fn previous_value_area_tpo_config(&self) -> super::tpo::Config {
        super::tpo::Config {
            ticks_per_row: u32::from(self.previous_value_area_ticks.max(1)),
            block_size: self.previous_value_area_block_size,
            session_start_minutes_utc: self.previous_value_area_session_start_minutes_utc,
            value_area_percent: self.previous_value_area_value_area_percent,
            ..super::tpo::Config::default()
        }
    }

    pub fn normalized_footprint_summary_abnormal_multiplier(&self) -> f32 {
        if self.footprint_summary_abnormal_multiplier.is_finite() {
            self.footprint_summary_abnormal_multiplier.clamp(
                Self::FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_MIN,
                Self::FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_MAX,
            )
        } else {
            Self::FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_DEFAULT
        }
    }

    pub const DAILY_DELTA_TICK_PRESETS: [TickMultiplier; 12] = [
        TickMultiplier(1),
        TickMultiplier(2),
        TickMultiplier(5),
        TickMultiplier(10),
        TickMultiplier(25),
        TickMultiplier(50),
        TickMultiplier(100),
        TickMultiplier(200),
        TickMultiplier(500),
        TickMultiplier(1000),
        TickMultiplier(1500),
        TickMultiplier(2000),
    ];
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_labels_always_visible: false,
            show_footprint_candles: true,
            show_footprint_summary: false,
            footprint_summary_abnormal_multiplier:
                Self::FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_DEFAULT,
            show_renko_wicks: true,
            daily_delta_ticks: 10,
            daily_delta_days: 4,
            previous_value_area_ticks: 10,
            previous_value_area_block_size: super::tpo::BlockSize::default(),
            previous_value_area_session_start_minutes_utc: 0,
            previous_value_area_value_area_percent: 70,
            liquidity_heatmap_order_size_filter: 0.0,
            large_trades_min_usd: Self::LARGE_TRADES_MIN_USD_DEFAULT,
            vpvr_ticks: 10,
        }
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub enum ClusterScaling {
    /// Scale based on the maximum quantity in the visible range.
    VisibleRange,
    /// Blend global VisibleRange and per-cluster Individual using a weight in [0.0, 1.0].
    /// weight = fraction of global contribution (1.0 == all-global, 0.0 == all-individual).
    Hybrid { weight: f32 },
    /// Scale based only on the maximum quantity inside the datapoint (per-candle).
    #[default]
    Datapoint,
}

impl ClusterScaling {
    pub const ALL: [ClusterScaling; 3] = [
        ClusterScaling::VisibleRange,
        ClusterScaling::Hybrid { weight: 0.2 },
        ClusterScaling::Datapoint,
    ];

    /// Blend the global visible-range max qty with the per-candle individual max qty
    /// according to the scaling strategy.
    pub fn effective_qty(self, visible_max: f64, individual_max: Qty) -> f64 {
        match self {
            ClusterScaling::VisibleRange => Qty::scale_or_one(visible_max),
            ClusterScaling::Datapoint => individual_max.to_scale_or_one(),
            ClusterScaling::Hybrid { weight } => {
                let w = weight.clamp(0.0, 1.0) as f64;
                Qty::scale_or_one(visible_max * w + individual_max.to_f64() * (1.0 - w))
            }
        }
    }
}

impl std::fmt::Display for ClusterScaling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClusterScaling::VisibleRange => write!(f, "Visible Range"),
            ClusterScaling::Hybrid { weight } => write!(f, "Hybrid (weight: {:.2})", weight),
            ClusterScaling::Datapoint => write!(f, "Per-candle"),
        }
    }
}

impl std::cmp::Eq for ClusterScaling {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FootprintStudy {
    NPoC {
        lookback: usize,
    },
    Imbalance {
        threshold: usize,
        color_scale: Option<usize>,
        ignore_zeros: bool,
    },
}

impl FootprintStudy {
    pub fn is_same_type(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (FootprintStudy::NPoC { .. }, FootprintStudy::NPoC { .. })
                | (
                    FootprintStudy::Imbalance { .. },
                    FootprintStudy::Imbalance { .. }
                )
        )
    }
}

impl FootprintStudy {
    pub const ALL: [FootprintStudy; 2] = [
        FootprintStudy::NPoC { lookback: 80 },
        FootprintStudy::Imbalance {
            threshold: 200,
            color_scale: Some(400),
            ignore_zeros: true,
        },
    ];
}

impl std::fmt::Display for FootprintStudy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FootprintStudy::NPoC { .. } => write!(f, "Naked Point of Control"),
            FootprintStudy::Imbalance { .. } => write!(f, "Imbalance"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PointOfControl {
    pub price: Price,
    pub volume: Qty,
    pub status: NPoc,
}

impl Default for PointOfControl {
    fn default() -> Self {
        Self {
            price: Price::from_f32(0.0),
            volume: Qty::ZERO,
            status: NPoc::default(),
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::{ClusterKind, ClusterScaling, Config, KlineChartKind, RenkoConfig};
    use crate::chart::indicator::KlineIndicator;

    #[test]
    fn legacy_config_defaults_new_visual_settings() {
        let config: Config = serde_json::from_str(
            r#"{"data_labels_always_visible":true,"show_footprint_summary":false,"show_renko_wicks":true,"daily_delta_ticks":10,"daily_delta_days":4,"previous_value_area_ticks":10}"#,
        )
        .expect("legacy kline config should deserialize");

        assert!(config.show_footprint_candles);
        assert_eq!(config.liquidity_heatmap_order_size_filter, 0.0);
        assert_eq!(
            config.footprint_summary_abnormal_multiplier,
            Config::FOOTPRINT_SUMMARY_ABNORMAL_MULTIPLIER_DEFAULT
        );
    }

    #[test]
    fn cme_gaps_require_timestamp_keyed_candle_data() {
        let footprint = KlineChartKind::Footprint {
            clusters: ClusterKind::BidAsk,
            scaling: ClusterScaling::default(),
            studies: Vec::new(),
        };

        assert!(KlineChartKind::Candles.allows_indicator(KlineIndicator::CmeGap));
        assert!(footprint.allows_indicator(KlineIndicator::CmeGap));
        assert!(
            !KlineChartKind::Renko {
                config: RenkoConfig::default(),
            }
            .allows_indicator(KlineIndicator::CmeGap)
        );
        assert!(
            !KlineChartKind::Tpo {
                config: crate::chart::tpo::Config::default(),
            }
            .allows_indicator(KlineIndicator::CmeGap)
        );
    }
}

#[cfg(test)]
mod footprint_rebin_tests {
    use super::*;
    use exchange::{
        TickMultiplier, Trade, UnixMs,
        unit::{MinTicksize, Price, Qty},
    };

    const GROUPED_RENDER_BENCH_BARS: usize = 240;
    const GROUPED_RENDER_BENCH_LEVELS: usize = 400;

    fn trade(price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(1_000),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    #[test]
    fn rebin_merges_fine_ticks_onto_grouped_step() {
        let fine: PriceStep = MinTicksize::new(-1).into();
        let coarse = TickMultiplier(200).multiply_step(fine);
        let mut footprint = KlineTrades::new();
        footprint.add_trade_to_nearest_bin(&trade(77_500.0, 1.0, true), fine);
        footprint.add_trade_to_nearest_bin(&trade(77_500.1, 1.0, true), fine);
        footprint.add_trade_to_nearest_bin(&trade(77_505.0, 1.0, false), fine);
        assert_eq!(footprint.trades.len(), 3);

        footprint.rebin(coarse);
        assert_eq!(footprint.trades.len(), 1);
        let group = footprint.trades.values().next().expect("grouped level");
        assert_eq!(group.sell_qty.to_f64(), 2.0);
        assert_eq!(group.buy_qty.to_f64(), 1.0);
    }

    #[test]
    fn grouped_to_step_does_not_mutate_the_source_and_folds_rows() {
        let fine: PriceStep = MinTicksize::new(-1).into();
        let coarse = TickMultiplier(500).multiply_step(fine);
        let mut footprint = KlineTrades::new();
        footprint.add_trade_to_nearest_bin(&trade(77_200.0, 1.0, true), fine);
        footprint.add_trade_to_nearest_bin(&trade(77_210.0, 2.0, false), fine);
        footprint.add_trade_to_nearest_bin(&trade(77_220.0, 3.0, true), fine);
        assert_eq!(footprint.trades.len(), 3);

        let grouped = footprint.grouped_to_step(coarse);
        assert_eq!(footprint.trades.len(), 3);
        assert_eq!(grouped.trades.len(), 1);
        let group = grouped.trades.values().next().expect("grouped level");
        assert_eq!(group.sell_qty.to_f64(), 4.0);
        assert_eq!(group.buy_qty.to_f64(), 2.0);
    }

    #[test]
    fn render_grouping_borrows_aligned_rows_and_owns_regrouped_rows() {
        let base: PriceStep = MinTicksize::new(-1).into();
        let coarse = TickMultiplier(100).multiply_step(base);
        let mut aligned = KlineTrades::new();
        aligned.add_trade_to_nearest_bin(&trade(77_500.0, 1.0, false), coarse);
        assert!(matches!(
            aligned.grouped_to_step_cow(coarse),
            Cow::Borrowed(_)
        ));

        let mut unaligned = KlineTrades::new();
        unaligned.add_trade_to_nearest_bin(&trade(77_500.1, 1.0, false), base);
        assert!(matches!(
            unaligned.grouped_to_step_cow(coarse),
            Cow::Owned(_)
        ));
    }

    /// Storage sits on the feed's base tick; switching the display
    /// multiplier must regroup rows losslessly in both directions.
    #[test]
    fn multiplier_switches_regroup_base_tick_rows() {
        let base: PriceStep = MinTicksize::new(-1).into();
        let mut footprint = KlineTrades::new();
        for i in 0..11 {
            let price = 77_200.0 + (i as f64) * 10.0;
            footprint.add_trade_to_nearest_bin(&trade(price, 1.0, i % 2 == 0), base);
        }
        assert_eq!(footprint.trades.len(), 11);

        let step_200 = TickMultiplier(200).multiply_step(base);
        let step_500 = TickMultiplier(500).multiply_step(base);

        let grouped_200 = footprint.grouped_to_step(step_200);
        let grouped_500 = footprint.grouped_to_step(step_500);

        assert!(
            grouped_200.trades.len() > grouped_500.trades.len(),
            "200-tick grouping ({}) should show more rows than 500-tick ({})",
            grouped_200.trades.len(),
            grouped_500.trades.len()
        );
        assert_eq!(grouped_500.trades.len(), 3);
        assert_eq!(grouped_200.trades.len(), 6);
        // Source stays untouched so switching back restores finer rows.
        assert_eq!(footprint.trades.len(), 11);
    }

    #[test]
    fn max_cluster_qty_grouped_matches_merged_histogram() {
        let base: PriceStep = MinTicksize::new(-1).into();
        let step = TickMultiplier(200).multiply_step(base);
        let mut footprint = KlineTrades::new();
        for (i, qty) in [0.5f64, 2.0, 1.0, 3.5, 0.25].iter().enumerate() {
            let price = 77_200.0 + f64::from(i as u32) * 7.0;
            footprint.add_trade_to_nearest_bin(&trade(price, *qty, false), base);
        }

        let grouped = footprint.grouped_to_step(step);
        let highest = Price::from_f64(78_000.0);
        let lowest = Price::from_f64(76_000.0);
        let expected = grouped.max_qty_by(highest, lowest, |group| {
            group.max_cluster_qty(ClusterKind::Table)
        });
        let actual = footprint.max_cluster_qty_grouped(ClusterKind::Table, highest, lowest, step);
        assert_eq!(actual, expected);
    }

    #[test]
    fn grouped_trade_time_bounds_ignore_insertion_order() {
        let fine: PriceStep = MinTicksize::new(-1).into();
        let mut footprint = KlineTrades::new();
        let mut later = trade(77_500.0, 1.0, false);
        later.time = UnixMs::new(3_000);
        let mut earlier = trade(77_500.0, 1.0, true);
        earlier.time = UnixMs::new(1_000);
        footprint.add_trade_to_nearest_bin(&later, fine);
        footprint.add_trade_to_nearest_bin(&earlier, fine);

        let group = footprint.trades.values().next().expect("price group");
        assert_eq!(group.first_time, UnixMs::new(1_000));
        assert_eq!(group.last_time, UnixMs::new(3_000));
    }

    #[test]
    #[ignore = "manual grouped Footprint render-preparation benchmark"]
    fn benchmark_already_grouped_footprint_render_preparation() {
        let base: PriceStep = MinTicksize::new(-1).into();
        let display_step = TickMultiplier(100).multiply_step(base);
        let mut footprint = KlineTrades::new();
        for level in 0..GROUPED_RENDER_BENCH_LEVELS {
            footprint.add_trade_to_nearest_bin(
                &trade(70_000.0 + level as f64 * 10.0, 1.0, level % 2 == 0),
                display_step,
            );
        }
        footprint.calculate_poc();
        let footprints = vec![footprint; GROUPED_RENDER_BENCH_BARS];
        let highest = Price::from_f64(100_000.0);
        let lowest = Price::from_f64(0.0);

        let mut samples = Vec::new();
        let mut checksum = 0usize;
        for _ in 0..7 {
            let started = std::time::Instant::now();
            checksum = footprints
                .iter()
                .map(|footprint| {
                    let grouped = footprint.grouped_to_step_cow(display_step);
                    let max = footprint.max_cluster_qty_grouped(
                        ClusterKind::Table,
                        highest,
                        lowest,
                        display_step,
                    );
                    std::hint::black_box((&grouped, max));
                    grouped.trades.len()
                })
                .sum();
            samples.push(started.elapsed());
        }
        assert_eq!(
            checksum,
            GROUPED_RENDER_BENCH_BARS * GROUPED_RENDER_BENCH_LEVELS,
            "display rows changed"
        );
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        println!(
            "grouped Footprint render prep: bars={} levels={} median_ms={:.3} checksum={checksum}",
            GROUPED_RENDER_BENCH_BARS,
            GROUPED_RENDER_BENCH_LEVELS,
            median.as_secs_f64() * 1_000.0,
        );
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum NPoc {
    #[default]
    None,
    Naked,
    Filled {
        at: u64,
    },
}

impl NPoc {
    pub fn filled(&mut self, at: u64) {
        *self = NPoc::Filled { at };
    }

    pub fn unfilled(&mut self) {
        *self = NPoc::Naked;
    }
}
