use super::kline::ClusterKind;
use exchange::unit::PriceStep;
use serde::{Deserialize, Serialize};

/// Per-pane TRev settings. Price thresholds always use exchange minimum ticks;
/// footprint row aggregation is presentation only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub continuation_ticks: u32,
    pub reversal_ticks: u32,
    pub normalization_ms: u32,
    pub display: Display,
    pub ticks_per_row: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            continuation_ticks: 200,
            reversal_ticks: 300,
            normalization_ms: 0,
            display: Display::Candles,
            ticks_per_row: 20,
        }
    }
}

impl Config {
    pub const ROW_PRESETS: [u32; 12] = [1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1_000];
    pub const NORMALIZATION_PRESETS: [u32; 8] = [0, 100, 250, 500, 1_000, 2_000, 5_000, 10_000];

    pub fn normalized(self) -> Self {
        Self {
            continuation_ticks: self.continuation_ticks.clamp(1, 100_000),
            reversal_ticks: self.reversal_ticks.clamp(1, 100_000),
            normalization_ms: self.normalization_ms.min(10_000),
            ticks_per_row: self.ticks_per_row.clamp(1, 10_000),
            ..self
        }
    }

    pub fn same_construction(self, other: Self) -> bool {
        self.continuation_ticks == other.continuation_ticks
            && self.reversal_ticks == other.reversal_ticks
            && self.normalization_ms == other.normalization_ms
    }

    pub fn row_step(self, base: PriceStep) -> PriceStep {
        PriceStep {
            units: base
                .units
                .saturating_mul(i64::from(self.normalized().ticks_per_row)),
        }
    }
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TRev {}/{}",
            self.continuation_ticks, self.reversal_ticks
        )
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Display {
    #[default]
    Candles,
    BidAsk,
    Volume,
    Delta,
}

impl Display {
    pub const ALL: [Self; 4] = [Self::Candles, Self::BidAsk, Self::Volume, Self::Delta];

    pub fn clusters(self) -> Option<ClusterKind> {
        match self {
            Self::Candles => None,
            Self::BidAsk => Some(ClusterKind::BidAsk),
            Self::Volume => Some(ClusterKind::VolumeProfile),
            Self::Delta => Some(ClusterKind::DeltaProfile),
        }
    }
}

impl std::fmt::Display for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Candles => "Candles",
            Self::BidAsk => "Bid × Ask footprint",
            Self::Volume => "Volume footprint",
            Self::Delta => "Delta footprint",
        })
    }
}
