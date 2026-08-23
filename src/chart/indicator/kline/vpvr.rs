use super::KlineIndicatorImpl;
use super::daily_delta::delta_history_colors;
use super::footprint_history::draw_text;
use crate::chart::{Message, ViewState};

use std::cell::{Cell, RefCell};
use std::ops::RangeInclusive;

use data::aggr::time::TimeSeries;
use data::chart::PlotData;
use data::chart::kline::{KlineDataPoint, KlineTrades};
use exchange::{
    SizeUnit,
    adapter::MarketKind,
    unit::{Price, PriceStep, qty::volume_size_unit},
};
use iced::theme::palette::Extended;
use iced::widget::canvas;
use iced::{Alignment, Color, Element, Point, Rectangle, Size};
use rustc_hash::FxHashMap;

/// Hard cap so a fully zoomed-out price axis cannot emit millions of bars.
const MAX_VISIBLE_PRICE_LEVELS: usize = 4096;

#[derive(Clone, Copy, Default)]
struct LevelAcc {
    buy: f64,
    sell: f64,
}

impl LevelAcc {
    fn is_empty(self) -> bool {
        self.buy <= 0.0 && self.sell <= 0.0
    }

    fn total(self) -> f64 {
        self.buy + self.sell
    }

    fn delta(self) -> f64 {
        self.buy - self.sell
    }
}

struct OverlayCache {
    rebuild_rev: u64,
    group_step_units: i64,
    qty_is_quote: bool,
    first_key: u64,
    last_key: u64,
    levels: FxHashMap<i64, LevelAcc>,
    max_abs_delta: f64,
    poc: Option<i64>,
    live_key: u64,
    live_snapshot: Vec<(i64, f64, f64)>,
}

struct HoverLevel {
    y: f32,
    h: f32,
    x0: f32,
    x1: f32,
    buy: f64,
    sell: f64,
}

/// Volume Profile of the Visible Range: one aggregated bid/ask histogram
/// built from the footprints of every candle currently on screen.
///
/// The profile is keyed by the first and last *included candle*, not by the
/// pixel interval, so panning inside a bar or zooming without adding/removing
/// candles is free. Live ticks only replace the forming bar.
pub struct VisibleRangeProfileIndicator {
    rebuild_rev: u64,
    dirty_from: Cell<Option<u64>>,
    cache: RefCell<Option<OverlayCache>>,
    hover: RefCell<Vec<HoverLevel>>,
}

impl VisibleRangeProfileIndicator {
    pub fn new() -> Self {
        Self {
            rebuild_rev: 0,
            dirty_from: Cell::new(None),
            cache: RefCell::new(None),
            hover: RefCell::new(Vec::new()),
        }
    }

    fn bump_rebuild(&mut self) {
        self.rebuild_rev = self.rebuild_rev.wrapping_add(1);
        self.dirty_from.set(None);
    }

    fn mark_dirty_from(&mut self, time: u64) {
        let current = self.dirty_from.get();
        self.dirty_from
            .set(Some(current.map_or(time, |value| value.min(time))));
    }
}

impl KlineIndicatorImpl for VisibleRangeProfileIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.borrow_mut().take();
        self.hover.borrow_mut().clear();
    }

    fn clear_crosshair_caches(&mut self) {
        self.hover.borrow_mut().clear();
    }

    fn element<'a>(
        &'a self,
        _chart: &'a ViewState,
        _data_labels_always_visible: bool,
        _visible_range: RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        iced::widget::row![].into()
    }

    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.bump_rebuild();
    }

    fn on_insert_klines(&mut self, klines: &[exchange::Kline], _source: &PlotData<KlineDataPoint>) {
        if let Some(earliest) = klines.iter().map(|kline| kline.time.as_u64()).min() {
            self.mark_dirty_from(earliest);
        }
    }

    fn on_insert_trades(
        &mut self,
        trades: &[exchange::Trade],
        old_dp_len: usize,
        source: &PlotData<KlineDataPoint>,
    ) {
        if let PlotData::TickBased(tick_aggr) = source
            && old_dp_len != tick_aggr.datapoints.len()
        {
            self.bump_rebuild();
            return;
        }
        if let Some(earliest) = trades.iter().map(|trade| trade.time.as_u64()).min() {
            self.mark_dirty_from(earliest);
        }
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.bump_rebuild();
    }

    fn on_basis_change(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.bump_rebuild();
    }

    fn draw_overlay(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        data_source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        group_step: PriceStep,
    ) {
        self.hover.borrow_mut().clear();
        if group_step.units <= 0 {
            return;
        }

        let (earliest, latest) = chart.interval_range(&region);
        let Some(span) = visible_span(data_source, earliest, latest) else {
            return;
        };

        let (highest, lowest) = chart.price_range(&region);
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || chart.ticker_info.market_type() == MarketKind::InversePerps;
        let scaling = chart.scaling.max(0.01);
        let profile_w = (region.width * 0.14).clamp(36.0 / scaling, 140.0 / scaling);
        let spine_x = region.x + 6.0 / scaling;
        let (buy, sell) = delta_history_colors(palette);

        self.sync_cache(data_source, span, group_step.units, qty_is_quote);
        let cache = self.cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return;
        };
        if cache.levels.is_empty() || cache.max_abs_delta <= 0.0 {
            return;
        }

        draw_profile_backdrop(frame, palette, spine_x, profile_w, region, scaling);

        let min_bar_h = 1.0 / scaling;
        let gap = (3.0 / scaling).clamp(1.2 / scaling, 4.0 / scaling);
        let text_size = (12.0 / scaling).clamp(10.0, 14.0);
        let text_pad = 8.0 / scaling;
        let text_on_bar = if palette.is_dark {
            Color::from_rgb(0.95, 0.96, 0.98)
        } else {
            Color::from_rgb(0.08, 0.09, 0.11)
        };
        let mut hover = Vec::new();
        let mut labels = Vec::new();
        let mut drawn = 0usize;

        for (price_units, level) in &cache.levels {
            if drawn >= MAX_VISIBLE_PRICE_LEVELS {
                break;
            }
            let delta = level.delta();
            if delta == 0.0 {
                continue;
            }
            let price = Price::from_units(*price_units);
            let next = Price::from_units(price_units.saturating_add(group_step.units));
            if next < lowest || price > highest {
                continue;
            }
            let y_top = chart.price_to_y(next);
            let y_bot = chart.price_to_y(price);
            let row_h = (y_bot - y_top).abs().max(min_bar_h);
            if !row_h.is_finite() {
                continue;
            }
            let bar_h = (row_h - gap).max(min_bar_h);
            let y = y_top.min(y_bot) + (row_h - bar_h) * 0.5;
            let width = ((delta.abs() / cache.max_abs_delta) as f32 * profile_w).max(2.0 / scaling);
            let start_x = spine_x;
            let end_x = spine_x + width;
            let color = if delta >= 0.0 { buy } else { sell };
            frame.fill_rectangle(Point::new(start_x, y), Size::new(width, bar_h), color);
            hover.push(HoverLevel {
                y: y + bar_h * 0.5,
                h: bar_h,
                x0: start_x,
                x1: end_x,
                buy: level.buy,
                sell: level.sell,
            });
            let text = usd_label(delta);
            let text_w = estimate_text_width(&text, text_size);
            // Keep the glyph box inside the pane: short bars would otherwise
            // right-align past the left edge and clip the first characters.
            let min_right = region.x + text_pad + text_w;
            let text_x = (end_x - text_pad).max(min_right);
            labels.push((text, text_x, y + bar_h * 0.5));
            drawn += 1;
        }

        if let Some(poc_units) = cache.poc {
            let price = Price::from_units(poc_units);
            let next = Price::from_units(poc_units.saturating_add(group_step.units));
            if next >= lowest && price <= highest {
                let y = (chart.price_to_y(price) + chart.price_to_y(next)) * 0.5;
                frame.fill_rectangle(
                    Point::new(spine_x, y - 0.6 / scaling),
                    Size::new(profile_w, (1.2 / scaling).max(0.6)),
                    buy.scale_alpha(0.85),
                );
            }
        }

        for (text, x, y) in &labels {
            draw_text(
                frame,
                text,
                Point::new(*x, *y),
                text_size,
                text_on_bar,
                Alignment::End,
            );
        }

        *self.hover.borrow_mut() = hover;
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
        let hover = self.hover.borrow();
        let Some(level) = hover.iter().find(|level| {
            let half = level.h * 0.5;
            cursor.y >= level.y - half
                && cursor.y <= level.y + half
                && cursor.x >= level.x0
                && cursor.x <= level.x1
        }) else {
            return;
        };
        let scaling = chart.scaling.max(0.01);
        let (buy, sell) = delta_history_colors(palette);
        let label = palette.background.base.text;
        let text_size = (12.0 / scaling).clamp(10.0, 14.0);
        let x = level.x1 + 6.0 / scaling;
        let y = level.y;
        draw_text(
            frame,
            &format!("B {}", usd_label(level.buy)),
            Point::new(x, y - 12.0 / scaling),
            text_size,
            buy,
            Alignment::Start,
        );
        draw_text(
            frame,
            &format!("S {}", usd_label(level.sell)),
            Point::new(x, y),
            text_size,
            sell,
            Alignment::Start,
        );
        draw_text(
            frame,
            &format!("Δ {}", usd_label(level.buy - level.sell)),
            Point::new(x, y + 12.0 / scaling),
            text_size,
            label,
            Alignment::Start,
        );
    }
}

impl VisibleRangeProfileIndicator {
    fn sync_cache(
        &self,
        data_source: &PlotData<KlineDataPoint>,
        span: VisibleSpan,
        step_units: i64,
        qty_is_quote: bool,
    ) {
        let dirty_from = self.dirty_from.get();
        let history_in_range = dirty_from.is_some_and(|from| match data_source {
            // Live prints land on the forming candle (`span.last`). Anything
            // older is a visible-range backfill and must rebuild the sum.
            PlotData::TimeBased(_) => from < span.last,
            // Tick/Renko bricks are not time-keyed; live ticks only replace
            // the last visible brick via `refresh_live_bar`.
            PlotData::TickBased(_) => false,
        });

        enum SyncKind {
            LiveOnly,
            Shift { from: VisibleSpan },
            Rebuild,
        }
        let kind = match self.cache.borrow().as_ref() {
            None => SyncKind::Rebuild,
            Some(cache)
                if cache.rebuild_rev != self.rebuild_rev
                    || cache.group_step_units != step_units
                    || cache.qty_is_quote != qty_is_quote
                    || history_in_range =>
            {
                SyncKind::Rebuild
            }
            Some(cache) if cache.first_key == span.first && cache.last_key == span.last => {
                SyncKind::LiveOnly
            }
            Some(cache)
                if spans_overlap(
                    VisibleSpan {
                        first: cache.first_key,
                        last: cache.last_key,
                    },
                    span,
                ) =>
            {
                SyncKind::Shift {
                    from: VisibleSpan {
                        first: cache.first_key,
                        last: cache.last_key,
                    },
                }
            }
            Some(_) => SyncKind::Rebuild,
        };

        match kind {
            SyncKind::Rebuild => self.rebuild_cache(data_source, span, step_units, qty_is_quote),
            SyncKind::Shift { from } => {
                self.shift_cache(data_source, from, span, step_units, qty_is_quote);
            }
            SyncKind::LiveOnly => {}
        }

        self.dirty_from.set(None);
        if matches!(kind, SyncKind::LiveOnly) {
            self.refresh_live_bar(data_source, span.last, step_units, qty_is_quote);
        }
    }

    fn rebuild_cache(
        &self,
        data_source: &PlotData<KlineDataPoint>,
        span: VisibleSpan,
        step_units: i64,
        qty_is_quote: bool,
    ) {
        let mut levels = FxHashMap::default();
        for_visible_bars(data_source, span, |footprint| {
            add_footprint(&mut levels, footprint, step_units, qty_is_quote);
        });
        let (_, max_abs_delta, poc) = profile_metrics(&levels);
        let live_snapshot = footprint_snapshot(
            live_footprint(data_source, span.last),
            step_units,
            qty_is_quote,
        );
        *self.cache.borrow_mut() = Some(OverlayCache {
            rebuild_rev: self.rebuild_rev,
            group_step_units: step_units,
            qty_is_quote,
            first_key: span.first,
            last_key: span.last,
            levels,
            max_abs_delta,
            poc,
            live_key: span.last,
            live_snapshot,
        });
    }

    fn shift_cache(
        &self,
        data_source: &PlotData<KlineDataPoint>,
        from: VisibleSpan,
        to: VisibleSpan,
        step_units: i64,
        qty_is_quote: bool,
    ) {
        let mut cache = self.cache.borrow_mut();
        let Some(cache) = cache.as_mut() else {
            return;
        };
        if to.first > from.first {
            for_half_open(data_source, from.first, to.first, |footprint| {
                sub_footprint(&mut cache.levels, footprint, step_units, qty_is_quote);
            });
        }
        if to.last < from.last {
            for_half_open(
                data_source,
                to.last.saturating_add(1),
                from.last.saturating_add(1),
                |footprint| sub_footprint(&mut cache.levels, footprint, step_units, qty_is_quote),
            );
        }
        if to.first < from.first {
            for_half_open(data_source, to.first, from.first, |footprint| {
                add_footprint(&mut cache.levels, footprint, step_units, qty_is_quote);
            });
        }
        if to.last > from.last {
            for_half_open(
                data_source,
                from.last.saturating_add(1),
                to.last.saturating_add(1),
                |footprint| add_footprint(&mut cache.levels, footprint, step_units, qty_is_quote),
            );
        }
        cache.first_key = to.first;
        cache.last_key = to.last;
        cache.live_key = to.last;
        cache.live_snapshot = footprint_snapshot(
            live_footprint(data_source, to.last),
            step_units,
            qty_is_quote,
        );
        let (_, max_abs_delta, poc) = profile_metrics(&cache.levels);
        cache.max_abs_delta = max_abs_delta;
        cache.poc = poc;
    }

    fn refresh_live_bar(
        &self,
        data_source: &PlotData<KlineDataPoint>,
        live_key: u64,
        step_units: i64,
        qty_is_quote: bool,
    ) {
        let mut cache = self.cache.borrow_mut();
        let Some(cache) = cache.as_mut() else {
            return;
        };
        if cache.live_key != live_key {
            return;
        }
        sub_snapshot(&mut cache.levels, &cache.live_snapshot);
        let snapshot = footprint_snapshot(
            live_footprint(data_source, live_key),
            step_units,
            qty_is_quote,
        );
        add_snapshot(&mut cache.levels, &snapshot);
        cache.live_snapshot = snapshot;
        let (_, max_abs_delta, poc) = profile_metrics(&cache.levels);
        cache.max_abs_delta = max_abs_delta;
        cache.poc = poc;
    }
}

#[derive(Clone, Copy)]
struct VisibleSpan {
    first: u64,
    last: u64,
}

fn visible_span(
    data_source: &PlotData<KlineDataPoint>,
    earliest: u64,
    latest: u64,
) -> Option<VisibleSpan> {
    match data_source {
        PlotData::TimeBased(series) => time_span(series, earliest, latest),
        PlotData::TickBased(tick_aggr) => {
            if tick_aggr.datapoints.is_empty() {
                return None;
            }
            let base = tick_aggr.datapoints.len().saturating_sub(1);
            let earliest = earliest as usize;
            let latest = latest as usize;
            let (lower, upper) = base
                .checked_sub(earliest)
                .map(|upper| (base.saturating_sub(latest), upper))?;
            (lower <= upper).then_some(VisibleSpan {
                first: lower as u64,
                last: upper as u64,
            })
        }
    }
}

fn time_span(
    series: &TimeSeries<KlineDataPoint>,
    earliest: u64,
    latest: u64,
) -> Option<VisibleSpan> {
    if latest < earliest {
        return None;
    }
    let mut iter = series
        .datapoints
        .range(exchange::UnixMs::new(earliest)..=exchange::UnixMs::new(latest))
        .map(|(time, _)| time.as_u64());
    let first = iter.next()?;
    let last = iter.next_back().unwrap_or(first);
    Some(VisibleSpan { first, last })
}

fn for_visible_bars(
    data_source: &PlotData<KlineDataPoint>,
    span: VisibleSpan,
    mut visit: impl FnMut(&KlineTrades),
) {
    match data_source {
        PlotData::TimeBased(series) => {
            for (_, dp) in series
                .datapoints
                .range(exchange::UnixMs::new(span.first)..=exchange::UnixMs::new(span.last))
            {
                visit(&dp.footprint);
            }
        }
        PlotData::TickBased(tick_aggr) => {
            let lower = span.first as usize;
            let upper = span.last as usize;
            if let Some(slice) = tick_aggr.datapoints.get(lower..=upper) {
                for dp in slice {
                    visit(&dp.footprint);
                }
            }
        }
    }
}

fn live_footprint(data_source: &PlotData<KlineDataPoint>, key: u64) -> Option<&KlineTrades> {
    match data_source {
        PlotData::TimeBased(series) => series
            .datapoints
            .get(&exchange::UnixMs::new(key))
            .map(|dp| &dp.footprint),
        PlotData::TickBased(tick_aggr) => tick_aggr
            .datapoints
            .get(key as usize)
            .map(|dp| &dp.footprint),
    }
}

fn add_footprint(
    levels: &mut FxHashMap<i64, LevelAcc>,
    footprint: &KlineTrades,
    step_units: i64,
    qty_is_quote: bool,
) {
    if step_units <= 0 {
        return;
    }
    for (price, group) in &footprint.trades {
        let bucket = grouped_price(price.units, step_units);
        let entry = levels.entry(bucket).or_default();
        entry.buy += usd_notional(*price, group.buy_qty.to_f64(), qty_is_quote);
        entry.sell += usd_notional(*price, group.sell_qty.to_f64(), qty_is_quote);
    }
}

fn sub_footprint(
    levels: &mut FxHashMap<i64, LevelAcc>,
    footprint: &KlineTrades,
    step_units: i64,
    qty_is_quote: bool,
) {
    if step_units <= 0 {
        return;
    }
    for (price, group) in &footprint.trades {
        let bucket = grouped_price(price.units, step_units);
        let Some(entry) = levels.get_mut(&bucket) else {
            continue;
        };
        entry.buy =
            (entry.buy - usd_notional(*price, group.buy_qty.to_f64(), qty_is_quote)).max(0.0);
        entry.sell =
            (entry.sell - usd_notional(*price, group.sell_qty.to_f64(), qty_is_quote)).max(0.0);
        if entry.is_empty() {
            levels.remove(&bucket);
        }
    }
}

fn usd_notional(price: Price, qty: f64, qty_is_quote: bool) -> f64 {
    if qty_is_quote {
        qty
    } else {
        price.to_f64() * qty
    }
}

fn spans_overlap(left: VisibleSpan, right: VisibleSpan) -> bool {
    left.first <= right.last && right.first <= left.last
}

fn for_half_open(
    data_source: &PlotData<KlineDataPoint>,
    start: u64,
    end: u64,
    mut visit: impl FnMut(&KlineTrades),
) {
    if end <= start {
        return;
    }
    match data_source {
        PlotData::TimeBased(series) => {
            for (_, dp) in series
                .datapoints
                .range(exchange::UnixMs::new(start)..exchange::UnixMs::new(end))
            {
                visit(&dp.footprint);
            }
        }
        PlotData::TickBased(tick_aggr) => {
            let start = start as usize;
            let end = (end as usize).min(tick_aggr.datapoints.len());
            if start < end {
                for dp in &tick_aggr.datapoints[start..end] {
                    visit(&dp.footprint);
                }
            }
        }
    }
}

fn footprint_snapshot(
    footprint: Option<&KlineTrades>,
    step_units: i64,
    qty_is_quote: bool,
) -> Vec<(i64, f64, f64)> {
    let Some(footprint) = footprint else {
        return Vec::new();
    };
    if step_units <= 0 {
        return Vec::new();
    }
    let mut grouped: FxHashMap<i64, LevelAcc> = FxHashMap::default();
    add_footprint(&mut grouped, footprint, step_units, qty_is_quote);
    grouped
        .into_iter()
        .map(|(price, level)| (price, level.buy, level.sell))
        .collect()
}

fn add_snapshot(levels: &mut FxHashMap<i64, LevelAcc>, snapshot: &[(i64, f64, f64)]) {
    for (price, buy, sell) in snapshot {
        let entry = levels.entry(*price).or_default();
        entry.buy += *buy;
        entry.sell += *sell;
    }
}

fn sub_snapshot(levels: &mut FxHashMap<i64, LevelAcc>, snapshot: &[(i64, f64, f64)]) {
    for (price, buy, sell) in snapshot {
        let Some(entry) = levels.get_mut(price) else {
            continue;
        };
        entry.buy = (entry.buy - *buy).max(0.0);
        entry.sell = (entry.sell - *sell).max(0.0);
        if entry.is_empty() {
            levels.remove(price);
        }
    }
}

fn profile_metrics(levels: &FxHashMap<i64, LevelAcc>) -> (f64, f64, Option<i64>) {
    let mut max_total = 0.0_f64;
    let mut max_abs_delta = 0.0_f64;
    let mut poc = None;
    for (price, level) in levels {
        let total = level.total();
        if total > max_total {
            max_total = total;
            poc = Some(*price);
        }
        max_abs_delta = max_abs_delta.max(level.delta().abs());
    }
    (max_total, max_abs_delta, poc)
}

fn grouped_price(units: i64, step_units: i64) -> i64 {
    units.div_euclid(step_units) * step_units
}

fn draw_profile_backdrop(
    frame: &mut canvas::Frame,
    palette: &Extended,
    spine_x: f32,
    profile_w: f32,
    region: Rectangle,
    scaling: f32,
) {
    let segments = 12;
    let segment_w = profile_w / segments as f32;
    for i in 0..segments {
        let t = i as f32 / (segments - 1) as f32;
        let alpha = 0.18 + 0.55 * (1.0 - t).powf(1.6);
        frame.fill_rectangle(
            Point::new(spine_x + i as f32 * segment_w, region.y),
            Size::new(segment_w, region.height),
            palette.background.weakest.color.scale_alpha(alpha),
        );
    }
    frame.fill_rectangle(
        Point::new(spine_x, region.y),
        Size::new(1.0 / scaling, region.height),
        palette.background.strong.color.scale_alpha(0.7),
    );
}

fn estimate_text_width(text: &str, size: f32) -> f32 {
    text.len() as f32 * size * 0.62
}

fn usd_label(value: f64) -> String {
    let abs = value.abs();
    let sign = if value < 0.0 { "-" } else { "" };
    if abs >= 1_000_000_000.0 {
        format!("{sign}{}b", trim_usd(abs / 1_000_000_000.0))
    } else if abs >= 1_000_000.0 {
        format!("{sign}{}m", trim_usd(abs / 1_000_000.0))
    } else if abs >= 1_000.0 {
        format!("{sign}{}k", trim_usd(abs / 1_000.0))
    } else {
        format!("{sign}{abs:.0}")
    }
}

fn trim_usd(value: f64) -> String {
    let text = format!("{value:.1}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::unit::Qty;
    use exchange::{Trade, UnixMs};

    fn trade(price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(1),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    fn footprint(trades: &[Trade]) -> KlineTrades {
        let step = PriceStep {
            units: Price::from_f64(0.1).units,
        };
        let mut footprint = KlineTrades::new();
        for trade in trades {
            footprint.add_trade_to_nearest_bin(trade, step);
        }
        footprint
    }

    #[test]
    fn visible_candles_sum_buy_and_sell_at_the_same_price() {
        let step = Price::from_f64(0.1).units;
        let mut levels = FxHashMap::default();
        add_footprint(
            &mut levels,
            &footprint(&[trade(100.0, 2.0, false), trade(100.0, 1.0, true)]),
            step,
            false,
        );
        add_footprint(
            &mut levels,
            &footprint(&[trade(100.0, 3.0, false), trade(100.1, 4.0, true)]),
            step,
            false,
        );

        let at_100 = levels[&Price::from_f64(100.0).units];
        assert!((at_100.buy - 500.0).abs() < 1e-6);
        assert!((at_100.sell - 100.0).abs() < 1e-6);
        assert!((at_100.delta() - 400.0).abs() < 1e-6);

        let at_101 = levels[&Price::from_f64(100.1).units];
        assert!((at_101.sell - 400.4).abs() < 1e-6);
        assert!(at_101.buy.abs() < 1e-6);
    }

    #[test]
    fn tick_grouping_folds_adjacent_prices() {
        let native = Price::from_f64(0.1).units;
        let grouped = native * 10;
        let mut levels = FxHashMap::default();
        add_footprint(
            &mut levels,
            &footprint(&[
                trade(100.0, 1.0, false),
                trade(100.5, 2.0, false),
                trade(101.0, 3.0, true),
            ]),
            grouped,
            false,
        );
        assert_eq!(levels.len(), 2);
        let low = levels[&grouped_price(Price::from_f64(100.0).units, grouped)];
        assert!((low.buy - 301.0).abs() < 1e-6);
        let high = levels[&grouped_price(Price::from_f64(101.0).units, grouped)];
        assert!((high.sell - 303.0).abs() < 1e-6);
    }

    #[test]
    fn live_snapshot_replace_does_not_double_count() {
        let step = Price::from_f64(0.1).units;
        let mut levels = FxHashMap::default();
        let first = footprint(&[trade(100.0, 1.0, false)]);
        add_footprint(&mut levels, &first, step, false);
        let snapshot = footprint_snapshot(Some(&first), step, false);
        sub_snapshot(&mut levels, &snapshot);
        let second = footprint(&[trade(100.0, 5.0, false), trade(100.0, 2.0, true)]);
        add_footprint(&mut levels, &second, step, false);
        let level = levels[&Price::from_f64(100.0).units];
        assert!((level.buy - 500.0).abs() < 1e-6);
        assert!((level.sell - 200.0).abs() < 1e-6);
    }

    #[test]
    fn subtracting_a_bar_undoes_its_contribution() {
        let step = Price::from_f64(0.1).units;
        let left = footprint(&[trade(100.0, 2.0, false), trade(100.0, 1.0, true)]);
        let right = footprint(&[trade(100.0, 4.0, false)]);
        let mut levels = FxHashMap::default();
        add_footprint(&mut levels, &left, step, false);
        add_footprint(&mut levels, &right, step, false);
        sub_footprint(&mut levels, &left, step, false);
        let remaining = levels[&Price::from_f64(100.0).units];
        assert!((remaining.buy - 400.0).abs() < 1e-6);
        assert!(remaining.sell.abs() < 1e-6);
    }

    #[test]
    fn poc_is_the_highest_volume_level() {
        let step = Price::from_f64(0.1).units;
        let mut levels = FxHashMap::default();
        add_footprint(
            &mut levels,
            &footprint(&[
                trade(100.0, 1.0, false),
                trade(100.5, 8.0, true),
                trade(101.0, 2.0, false),
            ]),
            step,
            false,
        );
        let (max_total, max_abs_delta, poc) = profile_metrics(&levels);
        assert!((max_total - 804.0).abs() < 1e-6);
        assert!((max_abs_delta - 804.0).abs() < 1e-6);
        assert_eq!(poc, Some(Price::from_f64(100.5).units));
    }

    #[test]
    fn usd_label_compacts_thousands_and_millions() {
        assert_eq!(usd_label(2_000_000.0), "2m");
        assert_eq!(usd_label(2_100_000.0), "2.1m");
        assert_eq!(usd_label(-100_000.0), "-100k");
        assert_eq!(usd_label(850.0), "850");
    }

    #[test]
    fn quote_normalized_quantity_is_not_multiplied_by_price_again() {
        let step = Price::from_f64(0.1).units;
        let mut levels = FxHashMap::default();
        let quote_footprint = footprint(&[trade(100.0, 250.0, false), trade(100.0, 125.0, true)]);

        add_footprint(&mut levels, &quote_footprint, step, true);

        let level = levels[&Price::from_f64(100.0).units];
        assert_eq!(level.buy, 250.0);
        assert_eq!(level.sell, 125.0);
        assert_eq!(level.delta(), 125.0);

        sub_footprint(&mut levels, &quote_footprint, step, true);
        assert!(levels.is_empty());
    }
}
