use exchange::{TickMultiplier, Ticker, TickerInfo, Timeframe};
use serde::{Deserialize, Serialize};

use crate::aggregation::AggregateFeedId;
use crate::chart::{comparison, heatmap, kline};
use crate::panel::{ladder, timeandsales};
use crate::stream::PersistStreamKind;
use crate::util::ok_or_default;

use crate::chart::{
    Basis, ViewConfig,
    heatmap::HeatmapStudy,
    indicator::{HeatmapIndicator, KlineIndicator},
    kline::KlineChartKind,
};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Pane {
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Pane>,
        b: Box<Pane>,
    },
    Starter {
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    HeatmapChart {
        layout: ViewConfig,
        #[serde(deserialize_with = "ok_or_default", default)]
        studies: Vec<HeatmapStudy>,
        #[serde(deserialize_with = "ok_or_default", default)]
        stream_type: Vec<PersistStreamKind>,
        #[serde(deserialize_with = "ok_or_default")]
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        indicators: Vec<HeatmapIndicator>,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    ShaderHeatmap {
        #[serde(deserialize_with = "ok_or_default", default)]
        studies: Vec<HeatmapStudy>,
        #[serde(deserialize_with = "ok_or_default", default)]
        stream_type: Vec<PersistStreamKind>,
        #[serde(deserialize_with = "ok_or_default")]
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        indicators: Vec<HeatmapIndicator>,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    KlineChart {
        layout: ViewConfig,
        kind: KlineChartKind,
        #[serde(deserialize_with = "ok_or_default", default)]
        stream_type: Vec<PersistStreamKind>,
        #[serde(deserialize_with = "ok_or_default")]
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        indicators: Vec<KlineIndicator>,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    ComparisonChart {
        stream_type: Vec<PersistStreamKind>,
        #[serde(deserialize_with = "ok_or_default")]
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    TimeAndSales {
        stream_type: Vec<PersistStreamKind>,
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
    Ladder {
        stream_type: Vec<PersistStreamKind>,
        settings: Settings,
        #[serde(deserialize_with = "ok_or_default", default)]
        link_group: Option<LinkGroup>,
    },
}

impl Default for Pane {
    fn default() -> Self {
        Pane::Starter { link_group: None }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Settings {
    pub tick_multiply: Option<exchange::TickMultiplier>,
    pub visual_config: Option<VisualConfig>,
    pub selected_basis: Option<Basis>,
    pub aggregate_feed: Option<AggregateFeedId>,
    /// Explicit aggregate-source selection. `None` means every available
    /// source; a non-empty list supports both mixed and single-venue views.
    pub aggregate_sources: Option<Vec<Ticker>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum LinkGroup {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
}

impl LinkGroup {
    pub const ALL: [LinkGroup; 9] = [
        LinkGroup::A,
        LinkGroup::B,
        LinkGroup::C,
        LinkGroup::D,
        LinkGroup::E,
        LinkGroup::F,
        LinkGroup::G,
        LinkGroup::H,
        LinkGroup::I,
    ];
}

impl std::fmt::Display for LinkGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = match self {
            LinkGroup::A => "1",
            LinkGroup::B => "2",
            LinkGroup::C => "3",
            LinkGroup::D => "4",
            LinkGroup::E => "5",
            LinkGroup::F => "6",
            LinkGroup::G => "7",
            LinkGroup::H => "8",
            LinkGroup::I => "9",
        };
        write!(f, "{c}")
    }
}

/// Defines the specific configuration for different types of pane settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum VisualConfig {
    Heatmap(heatmap::Config),
    TimeAndSales(timeandsales::Config),
    Kline(kline::Config),
    Ladder(ladder::Config),
    Comparison(comparison::Config),
}

impl VisualConfig {
    pub fn heatmap(&self) -> Option<heatmap::Config> {
        match self {
            Self::Heatmap(cfg) => Some(*cfg),
            _ => None,
        }
    }

    pub fn time_and_sales(&self) -> Option<timeandsales::Config> {
        match self {
            Self::TimeAndSales(cfg) => Some(*cfg),
            _ => None,
        }
    }

    pub fn kline(&self) -> Option<kline::Config> {
        match self {
            Self::Kline(cfg) => Some(*cfg),
            _ => None,
        }
    }

    pub fn ladder(&self) -> Option<ladder::Config> {
        match self {
            Self::Ladder(cfg) => Some(*cfg),
            _ => None,
        }
    }

    pub fn comparison(&self) -> Option<comparison::Config> {
        match self {
            Self::Comparison(cfg) => Some(cfg.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    Starter,
    HeatmapChart,
    ShaderHeatmap,
    FootprintChart,
    RenkoChart,
    TpoChart,
    CandlestickChart,
    ComparisonChart,
    TimeAndSales,
    Ladder,
}

impl ContentKind {
    pub const ALL: [ContentKind; 10] = [
        ContentKind::Starter,
        ContentKind::HeatmapChart,
        ContentKind::ShaderHeatmap,
        ContentKind::FootprintChart,
        ContentKind::RenkoChart,
        ContentKind::TpoChart,
        ContentKind::CandlestickChart,
        ContentKind::ComparisonChart,
        ContentKind::TimeAndSales,
        ContentKind::Ladder,
    ];

    /// Whether this surface consumes the reusable aggregate market-data feed.
    /// Add future aggregate-aware tools here; their stream planning stays in
    /// the shared aggregation layer.
    pub fn supports_aggregate_feed(self) -> bool {
        matches!(self, ContentKind::TpoChart)
    }
}

impl std::fmt::Display for ContentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ContentKind::Starter => "Starter Pane",
            ContentKind::HeatmapChart => "Heatmap Chart (Legacy)",
            ContentKind::ShaderHeatmap => "Heatmap Chart",
            ContentKind::FootprintChart => "Footprint · Executed Trades",
            ContentKind::RenkoChart => "Renko Chart",
            ContentKind::TpoChart => "TPO · Market Profile",
            ContentKind::CandlestickChart => "Candlestick Chart",
            ContentKind::ComparisonChart => "Comparison Chart",
            ContentKind::TimeAndSales => "Time&Sales",
            ContentKind::Ladder => "DOM/Ladder",
        };
        write!(f, "{s}")
    }
}

#[derive(Clone, Copy)]
pub struct PaneSetup {
    pub ticker_info: exchange::TickerInfo,
    pub basis: Option<Basis>,
    pub tick_multiplier: Option<TickMultiplier>,
    pub price_step: exchange::unit::PriceStep,
    pub depth_aggr: exchange::adapter::StreamTicksize,
    pub push_freq: exchange::PushFrequency,
}

impl PaneSetup {
    pub fn new(
        content_kind: ContentKind,
        base_ticker: TickerInfo,
        prev_base_ticker: Option<TickerInfo>,
        current_basis: Option<Basis>,
        current_tick_multiplier: Option<TickMultiplier>,
    ) -> Self {
        let exchange = base_ticker.ticker.exchange;

        let is_client_aggr = exchange.is_depth_client_aggr();
        let prev_is_client_aggr = prev_base_ticker
            .map(|ti| ti.ticker.exchange.is_depth_client_aggr())
            .unwrap_or(is_client_aggr);

        let basis =
            match content_kind {
                ContentKind::HeatmapChart | ContentKind::ShaderHeatmap => {
                    let current = current_basis.and_then(|b| match b {
                        Basis::Time(tf) if exchange.supports_heatmap_timeframe(tf) => Some(b),
                        _ => None,
                    });

                    Some(current.unwrap_or_else(|| Basis::default_heatmap_time(Some(base_ticker))))
                }
                ContentKind::Ladder => Some(
                    current_basis.unwrap_or_else(|| Basis::default_heatmap_time(Some(base_ticker))),
                ),
                ContentKind::FootprintChart => {
                    let current = current_basis.and_then(|b| match b {
                        Basis::Time(tf) if exchange.supports_kline_timeframe(tf) => Some(b),
                        Basis::Tick(_) => Some(b),
                        _ => None,
                    });

                    Some(current.unwrap_or_else(|| {
                        Basis::default_kline_time(Some(base_ticker), Timeframe::M5)
                    }))
                }
                ContentKind::RenkoChart | ContentKind::TpoChart => {
                    Some(Basis::Tick(crate::aggr::TickCount(1)))
                }
                ContentKind::CandlestickChart | ContentKind::ComparisonChart => {
                    let current = current_basis.and_then(|b| match b {
                        Basis::Time(tf) if exchange.supports_kline_timeframe(tf) => Some(b),
                        _ => None,
                    });

                    Some(current.unwrap_or_else(|| {
                        Basis::default_kline_time(Some(base_ticker), Timeframe::M15)
                    }))
                }
                ContentKind::Starter | ContentKind::TimeAndSales => None,
            };

        let tick_multiplier = match content_kind {
            ContentKind::HeatmapChart | ContentKind::Ladder | ContentKind::ShaderHeatmap => {
                let tm = if !is_client_aggr && prev_is_client_aggr {
                    TickMultiplier(10)
                } else if let Some(tm) = current_tick_multiplier {
                    tm
                } else if is_client_aggr {
                    TickMultiplier(5)
                } else {
                    TickMultiplier(10)
                };
                Some(tm)
            }
            ContentKind::FootprintChart => {
                Some(current_tick_multiplier.unwrap_or(TickMultiplier(50)))
            }
            ContentKind::RenkoChart | ContentKind::TpoChart => None,
            ContentKind::CandlestickChart
            | ContentKind::ComparisonChart
            | ContentKind::TimeAndSales
            | ContentKind::Starter => current_tick_multiplier,
        };

        let price_step = match tick_multiplier {
            Some(tm) => tm.multiply_with_min_tick_step(base_ticker),
            None => base_ticker.min_ticksize.into(),
        };

        let depth_aggr = exchange.stream_ticksize(tick_multiplier, TickMultiplier(50));

        let push_freq = match content_kind {
            ContentKind::HeatmapChart if exchange.is_custom_push_freq() => match basis {
                Some(Basis::Time(tf)) if exchange.supports_heatmap_timeframe(tf) => {
                    exchange::PushFrequency::Custom(tf)
                }
                _ => exchange::PushFrequency::ServerDefault,
            },
            _ => exchange::PushFrequency::ServerDefault,
        };

        Self {
            ticker_info: base_ticker,
            basis,
            tick_multiplier,
            price_step,
            depth_aggr,
            push_freq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;

    #[test]
    fn aggregate_source_selection_roundtrips_and_defaults_for_old_layouts() {
        let selected = vec![
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            Ticker::new("BTC", Exchange::HyperliquidLinear),
        ];
        let settings = Settings {
            aggregate_feed: Some(AggregateFeedId::BtcUsdtPerpetual),
            aggregate_sources: Some(selected.clone()),
            ..Settings::default()
        };

        let encoded = serde_json::to_string(&settings).expect("settings serialize");
        let decoded: Settings = serde_json::from_str(&encoded).expect("settings deserialize");
        assert_eq!(decoded.aggregate_sources, Some(selected));

        let legacy: Settings = serde_json::from_str("{}").expect("legacy settings deserialize");
        assert_eq!(legacy.aggregate_sources, None);
    }
}
