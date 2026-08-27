use std::fmt::{self, Debug, Display};

use enum_map::Enum;
use exchange::adapter::MarketKind;
use serde::{Deserialize, Serialize};

pub trait Indicator: PartialEq + Display + 'static {
    fn for_market(market: MarketKind) -> &'static [Self]
    where
        Self: Sized;
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, Eq, Enum)]
pub enum KlineIndicator {
    Volume,
    BarAnalysis,
    CumulativeDelta,
    OpenInterest,
    FootprintHistory,
    DailyDelta,
    CmeGap,
    PreviousValueArea,
    LiquidityHeatmap,
    LargeTrades,
    VisibleRangeProfile,
}

impl Indicator for KlineIndicator {
    fn for_market(market: MarketKind) -> &'static [Self] {
        match market {
            MarketKind::Spot => &Self::FOR_SPOT,
            MarketKind::LinearPerps | MarketKind::InversePerps => &Self::FOR_PERPS,
        }
    }
}

impl KlineIndicator {
    // Indicator togglers on UI menus depend on these arrays.
    // Every variant needs to be in either SPOT, PERPS or both.
    /// Indicators that can be used with spot market tickers
    const FOR_SPOT: [KlineIndicator; 8] = [
        KlineIndicator::Volume,
        KlineIndicator::BarAnalysis,
        KlineIndicator::CumulativeDelta,
        KlineIndicator::DailyDelta,
        KlineIndicator::CmeGap,
        KlineIndicator::PreviousValueArea,
        KlineIndicator::LargeTrades,
        KlineIndicator::VisibleRangeProfile,
    ];
    /// Indicators that can be used with perpetual swap market tickers
    const FOR_PERPS: [KlineIndicator; 10] = [
        KlineIndicator::Volume,
        KlineIndicator::BarAnalysis,
        KlineIndicator::CumulativeDelta,
        KlineIndicator::OpenInterest,
        KlineIndicator::DailyDelta,
        KlineIndicator::CmeGap,
        KlineIndicator::PreviousValueArea,
        KlineIndicator::LiquidityHeatmap,
        KlineIndicator::LargeTrades,
        KlineIndicator::VisibleRangeProfile,
    ];

    /// Overlay drawn on the main chart instead of a subplot row.
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            Self::DailyDelta
                | Self::CmeGap
                | Self::PreviousValueArea
                | Self::LiquidityHeatmap
                | Self::LargeTrades
                | Self::VisibleRangeProfile
        )
    }

    /// Overlay that needs executed trades bucketed onto the visible candles
    /// (live tape + visible-range history), without the UTC-day trade book.
    pub fn needs_visible_trades(self) -> bool {
        matches!(self, Self::VisibleRangeProfile)
    }

    /// Needs the shared multi-venue daily trade history pipeline.
    pub fn needs_trade_history(self) -> bool {
        matches!(
            self,
            Self::FootprintHistory | Self::DailyDelta | Self::CumulativeDelta | Self::LargeTrades
        )
    }
}

impl Display for KlineIndicator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KlineIndicator::Volume => write!(f, "Volume"),
            KlineIndicator::BarAnalysis => write!(f, "Bar Analysis"),
            KlineIndicator::CumulativeDelta => write!(f, "CVD"),
            KlineIndicator::OpenInterest => write!(f, "Open Interest"),
            KlineIndicator::FootprintHistory => write!(f, "Footprint History"),
            KlineIndicator::DailyDelta => write!(f, "Daily Delta"),
            KlineIndicator::CmeGap => write!(f, "CME Gaps"),
            KlineIndicator::PreviousValueArea => write!(f, "Previous Value Areas"),
            KlineIndicator::LiquidityHeatmap => write!(f, "Liquidity Heatmap"),
            KlineIndicator::LargeTrades => write!(f, "Large Trades"),
            KlineIndicator::VisibleRangeProfile => write!(f, "VPVR"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, Eq, Enum)]
pub enum HeatmapIndicator {
    Volume,
}

impl Indicator for HeatmapIndicator {
    fn for_market(market: MarketKind) -> &'static [Self] {
        match market {
            MarketKind::Spot => &Self::FOR_SPOT,
            MarketKind::LinearPerps | MarketKind::InversePerps => &Self::FOR_PERPS,
        }
    }
}

impl HeatmapIndicator {
    // Indicator togglers on UI menus depend on these arrays.
    // Every variant needs to be in either SPOT, PERPS or both.
    /// Indicators that can be used with spot market tickers
    const FOR_SPOT: [HeatmapIndicator; 1] = [HeatmapIndicator::Volume];
    /// Indicators that can be used with perpetual swap market tickers
    const FOR_PERPS: [HeatmapIndicator; 1] = [HeatmapIndicator::Volume];
}

impl Display for HeatmapIndicator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HeatmapIndicator::Volume => write!(f, "Volume"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// Temporary workaround,
/// represents any indicator type in the UI
pub enum UiIndicator {
    Heatmap(HeatmapIndicator),
    Kline(KlineIndicator),
}

impl From<KlineIndicator> for UiIndicator {
    fn from(k: KlineIndicator) -> Self {
        UiIndicator::Kline(k)
    }
}

impl From<HeatmapIndicator> for UiIndicator {
    fn from(h: HeatmapIndicator) -> Self {
        UiIndicator::Heatmap(h)
    }
}
