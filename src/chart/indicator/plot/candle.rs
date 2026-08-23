use std::ops::RangeInclusive;

use iced::{Point, Size, Theme, widget::canvas};

use crate::chart::{
    ViewState,
    indicator::plot::{Plot, PlotTooltip, Series, TooltipFn, YScale},
};

pub struct CandlePlot<O, H, L, C, T> {
    open: O,
    high: H,
    low: L,
    close: C,
    tooltip: Option<TooltipFn<T>>,
    padding: f32,
    bar_width_factor: f32,
}

impl<O, H, L, C, T> CandlePlot<O, H, L, C, T> {
    pub fn new(open: O, high: H, low: L, close: C) -> Self {
        Self {
            open,
            high,
            low,
            close,
            tooltip: None,
            padding: 0.08,
            bar_width_factor: 0.72,
        }
    }

    pub fn with_tooltip<F>(mut self, tooltip: F) -> Self
    where
        F: Fn(&T, Option<&T>) -> PlotTooltip + 'static,
    {
        self.tooltip = Some(Box::new(tooltip));
        self
    }
}

impl<S, O, H, L, C> Plot<S> for CandlePlot<O, H, L, C, S::Y>
where
    S: Series,
    O: Fn(&S::Y) -> f32,
    H: Fn(&S::Y) -> f32,
    L: Fn(&S::Y) -> f32,
    C: Fn(&S::Y) -> f32,
{
    fn y_extents(&self, datapoints: &S, range: RangeInclusive<u64>) -> Option<(f32, f32)> {
        let mut lowest = f32::MAX;
        let mut highest = f32::MIN;
        datapoints.for_each_in(range, |_, candle| {
            lowest = lowest.min((self.low)(candle));
            highest = highest.max((self.high)(candle));
        });
        (lowest != f32::MAX).then_some((lowest, highest))
    }

    fn adjust_extents(&self, min: f32, max: f32) -> (f32, f32) {
        let range = (max - min).max(max.abs() * 0.0001).max(1.0);
        let pad = range * self.padding;
        (min - pad, max + pad)
    }

    fn draw(
        &self,
        frame: &mut canvas::Frame,
        ctx: &ViewState,
        theme: &Theme,
        datapoints: &S,
        range: RangeInclusive<u64>,
        scale: &YScale,
    ) {
        let palette = theme.extended_palette();
        let width = (ctx.cell_width * self.bar_width_factor).max(1.0);

        datapoints.for_each_in(range, |x, candle| {
            let open = (self.open)(candle);
            let high = (self.high)(candle);
            let low = (self.low)(candle);
            let close = (self.close)(candle);
            let color = if close >= open {
                palette.success.strong.color
            } else {
                palette.danger.strong.color
            };
            let center_x = ctx.interval_to_x(x);
            let high_y = scale.to_y(high);
            let low_y = scale.to_y(low);
            frame.fill_rectangle(
                Point::new(center_x - 0.5, high_y),
                Size::new(1.0, (low_y - high_y).max(1.0)),
                color,
            );

            let open_y = scale.to_y(open);
            let close_y = scale.to_y(close);
            let top = open_y.min(close_y);
            frame.fill_rectangle(
                Point::new(center_x - width / 2.0, top),
                Size::new(width, (open_y - close_y).abs().max(1.0)),
                color,
            );
        });
    }

    fn tooltip_fn(&self) -> Option<&TooltipFn<S::Y>> {
        self.tooltip.as_ref()
    }
}
