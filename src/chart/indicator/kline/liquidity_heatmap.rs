use super::KlineIndicatorImpl;
use crate::chart::{Message, ViewState};

use iced::Element;

/// Marker implementation for the GPU liquidity layer drawn behind the main
/// kline canvas. Rendering and source-aware depth ingestion live on KlineChart.
pub struct LiquidityHeatmapIndicator;

impl KlineIndicatorImpl for LiquidityHeatmapIndicator {
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
}
