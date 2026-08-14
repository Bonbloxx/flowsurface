use crate::{
    chart::{Caches, Interaction, Message, ViewState, indicator::kline::KlineIndicatorImpl},
    connector::fetcher::{TradeFetchMode, trade_fetch_mode},
    style,
};

use data::{chart::PlotData, chart::kline::KlineDataPoint, util::abbr_large_numbers};
use exchange::{
    OpenInterest, Ticker, TickerInfo, Trade, UnixMs,
    adapter::Venue,
    unit::{Price, PriceStep},
};
use iced::widget::canvas::{self, Cache, Geometry};
use iced::{
    Alignment, Color, Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, mouse,
    widget::{Canvas, container, row, rule, space},
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;

const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
const FIVE_MIN_MS: u64 = 5 * 60 * 1_000;
const DAYS: usize = 3;

#[derive(Debug, Clone, Copy, Default)]
struct LevelStats {
    bid: f64,
    ask: f64,
}

impl LevelStats {
    fn volume(self) -> f64 {
        self.bid + self.ask
    }

    fn delta(self) -> f64 {
        self.ask - self.bid
    }

    fn merge(&mut self, other: Self) {
        self.bid += other.bid;
        self.ask += other.ask;
    }
}

#[derive(Debug, Clone, Copy)]
struct LargestTrade {
    time: UnixMs,
    is_sell: bool,
    price: f64,
    qty: f64,
    notional: f64,
}

#[derive(Debug, Clone, Default)]
struct DayStats {
    first: Option<(UnixMs, f64)>,
    last: Option<(UnixMs, f64)>,
    high: Option<f64>,
    low: Option<f64>,
    buy: f64,
    sell: f64,
    notional: f64,
    levels: BTreeMap<i64, LevelStats>,
    five_min_delta: BTreeMap<u64, f64>,
    largest_trade: Option<LargestTrade>,
}

impl DayStats {
    fn insert_trade(&mut self, trade: Trade) {
        let price = trade.price.to_f64();
        let qty = trade.qty.to_f64();
        let notional = price * qty;

        if self.first.is_none_or(|(time, _)| trade.time < time) {
            self.first = Some((trade.time, price));
        }
        if self.last.is_none_or(|(time, _)| trade.time > time) {
            self.last = Some((trade.time, price));
        }
        self.high = Some(self.high.map_or(price, |value| value.max(price)));
        self.low = Some(self.low.map_or(price, |value| value.min(price)));
        if trade.is_sell {
            self.sell += qty;
        } else {
            self.buy += qty;
        }
        self.notional += notional;

        let level = self.levels.entry(trade.price.units).or_default();
        if trade.is_sell {
            level.bid += qty;
        } else {
            level.ask += qty;
        }

        let bucket = trade.time.as_u64() / FIVE_MIN_MS * FIVE_MIN_MS;
        *self.five_min_delta.entry(bucket).or_default() += if trade.is_sell { -qty } else { qty };

        if self
            .largest_trade
            .is_none_or(|largest| notional > largest.notional)
        {
            self.largest_trade = Some(LargestTrade {
                time: trade.time,
                is_sell: trade.is_sell,
                price,
                qty,
                notional,
            });
        }
    }

    fn merge(&mut self, other: &Self) {
        if let Some(first) = other.first
            && self.first.is_none_or(|current| first.0 < current.0)
        {
            self.first = Some(first);
        }
        if let Some(last) = other.last
            && self.last.is_none_or(|current| last.0 > current.0)
        {
            self.last = Some(last);
        }
        if let Some(high) = other.high {
            self.high = Some(self.high.map_or(high, |value| value.max(high)));
        }
        if let Some(low) = other.low {
            self.low = Some(self.low.map_or(low, |value| value.min(low)));
        }
        self.buy += other.buy;
        self.sell += other.sell;
        self.notional += other.notional;
        for (price, level) in &other.levels {
            self.levels.entry(*price).or_default().merge(*level);
        }
        for (bucket, delta) in &other.five_min_delta {
            *self.five_min_delta.entry(*bucket).or_default() += delta;
        }
        if let Some(largest) = other.largest_trade
            && self
                .largest_trade
                .is_none_or(|current| largest.notional > current.notional)
        {
            self.largest_trade = Some(largest);
        }
    }

    fn volume(&self) -> f64 {
        self.buy + self.sell
    }

    fn delta(&self) -> f64 {
        self.buy - self.sell
    }

    fn cvd_range(&self) -> (f64, f64) {
        let mut current: f64 = 0.0;
        let mut low: f64 = 0.0;
        let mut high: f64 = 0.0;
        for delta in self.five_min_delta.values() {
            current += delta;
            low = low.min(current);
            high = high.max(current);
        }
        (low, high)
    }

    fn largest_bucket(&self, positive: bool) -> Option<(u64, f64)> {
        self.five_min_delta
            .iter()
            .filter(|(_, value)| {
                if positive {
                    **value > 0.0
                } else {
                    **value < 0.0
                }
            })
            .max_by(|left, right| {
                let left = if positive { *left.1 } else { -*left.1 };
                let right = if positive { *right.1 } else { -*right.1 };
                left.total_cmp(&right)
            })
            .map(|(time, value)| (*time, *value))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct OiDay {
    first: Option<(UnixMs, f64)>,
    last: Option<(UnixMs, f64)>,
}

impl OiDay {
    fn insert(&mut self, value: OpenInterest) {
        if self.first.is_none_or(|(time, _)| value.time < time) {
            self.first = Some((value.time, value.value));
        }
        if self.last.is_none_or(|(time, _)| value.time > time) {
            self.last = Some((value.time, value.value));
        }
    }

    fn delta(self) -> Option<f64> {
        Some(self.last?.1 - self.first?.1)
    }
}

#[derive(Debug, Default)]
struct SourceHistory {
    days: BTreeMap<u64, DayStats>,
    oi: BTreeMap<u64, OiDay>,
}

#[derive(Debug, Clone, Default)]
struct DisplayDay {
    stats: DayStats,
    oi_delta: Option<f64>,
}

pub struct FootprintHistoryIndicator {
    cache: Caches,
    sources: Vec<TickerInfo>,
    aggregate: bool,
    histories: FxHashMap<Ticker, SourceHistory>,
    history_cutoffs: FxHashMap<Ticker, UnixMs>,
    historical_started: FxHashSet<Ticker>,
    pending_live: FxHashMap<Ticker, Vec<Trade>>,
}

impl FootprintHistoryIndicator {
    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            sources: Vec::new(),
            aggregate: true,
            histories: FxHashMap::default(),
            history_cutoffs: FxHashMap::default(),
            historical_started: FxHashSet::default(),
            pending_live: FxHashMap::default(),
        }
    }

    fn active_sources(&self) -> &[TickerInfo] {
        if self.aggregate {
            &self.sources
        } else {
            self.sources.get(..1).unwrap_or(&[])
        }
    }

    fn display_days(&self, now: UnixMs) -> [DisplayDay; DAYS] {
        let today = day_start(now);
        std::array::from_fn(|index| {
            let day = today.saturating_sub(index as u64 * DAY_MS);
            let mut result = DisplayDay::default();
            let mut oi_delta = 0.0;
            let mut has_oi = false;
            for source in self.active_sources() {
                if let Some(history) = self.histories.get(&source.ticker) {
                    if let Some(stats) = history.days.get(&day) {
                        result.stats.merge(stats);
                    }
                    if let Some(delta) = history.oi.get(&day).and_then(|oi| oi.delta()) {
                        oi_delta += delta;
                        has_oi = true;
                    }
                }
            }
            result.oi_delta = has_oi.then_some(oi_delta);
            result
        })
    }

    fn source_label(&self) -> String {
        let names = self
            .active_sources()
            .iter()
            .map(|source| source.exchange().venue().to_string())
            .collect::<Vec<_>>();
        if self.aggregate && names.len() > 1 {
            format!("Aggregated · {}", names.join(" + "))
        } else {
            names
                .first()
                .cloned()
                .unwrap_or_else(|| "No venue".to_string())
        }
    }

    fn history_note(&self) -> Option<&'static str> {
        match trade_fetch_mode() {
            TradeFetchMode::Off => Some("Live data only · enable historical trades in Network"),
            TradeFetchMode::Exchange
                if self
                    .active_sources()
                    .iter()
                    .any(|source| source.exchange().venue() != Venue::Binance) =>
            {
                Some("Bybit / Hyperliquid history requires Server trade fetching")
            }
            TradeFetchMode::Server
                if self
                    .active_sources()
                    .iter()
                    .any(|source| source.exchange().venue() == Venue::Hyperliquid) =>
            {
                Some("OI Δ uses Binance / Bybit · HL history unavailable")
            }
            TradeFetchMode::Exchange | TradeFetchMode::Server => None,
        }
    }
}

impl KlineIndicatorImpl for FootprintHistoryIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
    }

    fn clear_crosshair_caches(&mut self) {}

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        _data_labels_always_visible: bool,
        _visible_range: std::ops::RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        let days = self.display_days(UnixMs::now());
        let canvas = Canvas::new(FootprintHistoryCanvas {
            cache: &self.cache.main,
            days,
            sources: self.source_label(),
            history_note: self.history_note(),
            min_tick: self
                .active_sources()
                .iter()
                .map(|source| PriceStep::from(source.min_ticksize))
                .min_by_key(|step| step.units)
                .unwrap_or(chart.tick_size),
        })
        .height(Length::Fill)
        .width(Length::Fill);

        row![
            canvas,
            rule::vertical(1).style(style::split_ruler),
            container(space::vertical()).width(chart.y_labels_width())
        ]
        .into()
    }

    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn configure_footprint_history(&mut self, sources: &[TickerInfo], aggregate: bool) {
        self.sources = sources.to_vec();
        self.aggregate = aggregate;
        self.history_cutoffs.clear();
        self.historical_started.clear();
        self.pending_live.clear();
        self.clear_all_caches();
    }

    fn prepare_footprint_history(&mut self, source: TickerInfo, cutoff: UnixMs) {
        self.history_cutoffs.insert(source.ticker, cutoff);
    }

    fn on_source_trades(&mut self, source: TickerInfo, trades: &[Trade], historical: bool) {
        if !self
            .sources
            .iter()
            .any(|candidate| candidate.ticker.same_market(&source.ticker))
        {
            return;
        }

        let source_key = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map_or(source.ticker, |candidate| candidate.ticker);

        let first_historical_batch = historical && self.historical_started.insert(source_key);
        let replay_live = if first_historical_batch {
            self.pending_live.remove(&source_key).unwrap_or_default()
        } else {
            Vec::new()
        };
        if first_historical_batch {
            self.histories.entry(source_key).or_default().days.clear();
        }

        let cutoff = self.history_cutoffs.get(&source_key).copied();
        let oldest = day_start(UnixMs::now()).saturating_sub((DAYS as u64 - 1) * DAY_MS);
        if !historical && !self.historical_started.contains(&source_key) {
            self.pending_live.entry(source_key).or_default().extend(
                trades
                    .iter()
                    .copied()
                    .filter(|trade| cutoff.is_some_and(|boundary| trade.time > boundary)),
            );
        }
        let history = self.histories.entry(source_key).or_default();
        for trade in trades.iter().chain(replay_live.iter()) {
            if trade.time.as_u64() < oldest
                || (!historical && cutoff.is_some_and(|boundary| trade.time <= boundary))
            {
                continue;
            }
            history
                .days
                .entry(day_start(trade.time))
                .or_default()
                .insert_trade(*trade);
        }
        history.days.retain(|day, _| *day >= oldest);
        self.clear_all_caches();
    }

    fn on_source_open_interest(&mut self, source: TickerInfo, values: &[OpenInterest]) {
        let Some(source_key) = self
            .sources
            .iter()
            .find(|candidate| candidate.ticker.same_market(&source.ticker))
            .map(|candidate| candidate.ticker)
        else {
            return;
        };
        let oldest = day_start(UnixMs::now()).saturating_sub((DAYS as u64 - 1) * DAY_MS);
        let history = self.histories.entry(source_key).or_default();
        for value in values {
            if value.time.as_u64() >= oldest {
                history
                    .oi
                    .entry(day_start(value.time))
                    .or_default()
                    .insert(*value);
            }
        }
        history.oi.retain(|day, _| *day >= oldest);
        self.clear_all_caches();
    }
}

struct FootprintHistoryCanvas<'a> {
    cache: &'a Cache,
    days: [DisplayDay; DAYS],
    sources: String,
    history_note: Option<&'static str>,
    min_tick: PriceStep,
}

impl canvas::Program<Message> for FootprintHistoryCanvas<'_> {
    type State = Interaction;

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            if bounds.width < 30.0 || bounds.height < 30.0 {
                return;
            }
            let width = bounds.width / DAYS as f32;
            let fg = palette.background.base.text;
            let muted = palette.background.strong.text.scale_alpha(0.72);
            let border = palette.background.strong.color;
            let buy = palette.success.base.color;
            let sell = palette.danger.base.color;
            let accent = palette.primary.base.color;
            let warning = palette.warning.base.color;
            let today = day_start(UnixMs::now());

            draw_text(
                frame,
                &self.sources,
                Point::new(6.0, 10.0),
                10.0,
                muted,
                Alignment::Start,
            );
            if let Some(note) = self.history_note {
                draw_text(
                    frame,
                    note,
                    Point::new(bounds.width - 6.0, 10.0),
                    9.0,
                    warning,
                    Alignment::End,
                );
            }

            let top = 22.0;
            let metrics_height = 178.0_f32.min((bounds.height * 0.48).max(116.0));
            let row_h = ((metrics_height - 26.0) / 9.0).clamp(10.0, 17.0);
            let table_top = top + metrics_height;

            for index in 0..DAYS {
                let left = width * index as f32;
                let right = left + width;
                if index > 0 {
                    frame.fill_rectangle(
                        Point::new(left, top),
                        Size::new(1.0, bounds.height - top),
                        border,
                    );
                }
                let day_start = today.saturating_sub(index as u64 * DAY_MS);
                let title = match index {
                    0 => "Current day",
                    1 => "Previous day",
                    _ => "Two days ago",
                };
                draw_text(
                    frame,
                    title,
                    Point::new(left + 7.0, top + 8.0),
                    11.0,
                    fg,
                    Alignment::Start,
                );
                draw_text(
                    frame,
                    &format_day(day_start),
                    Point::new(right - 7.0, top + 8.0),
                    9.0,
                    muted,
                    Alignment::End,
                );
                draw_day_metrics(
                    frame,
                    &self.days[index],
                    left + 7.0,
                    right - 7.0,
                    top + 24.0,
                    row_h,
                    fg,
                    muted,
                    buy,
                    sell,
                    accent,
                );
            }

            frame.fill_rectangle(
                Point::new(0.0, table_top),
                Size::new(bounds.width, 1.0),
                border,
            );
            let available_rows =
                ((bounds.height - table_top - 34.0) / 15.0).floor().max(1.0) as usize;
            let (step, rows) = shared_price_rows(&self.days, self.min_tick, available_rows);
            let step_label = format_price_step(step);

            for index in 0..DAYS {
                let left = width * index as f32;
                draw_text(
                    frame,
                    &format!("Daily footprint @ {step_label}"),
                    Point::new(left + 7.0, table_top + 10.0),
                    9.0,
                    fg,
                    Alignment::Start,
                );
                draw_table_header(frame, left, width, table_top + 24.0, muted);
            }

            let grouped = self
                .days
                .iter()
                .map(|day| group_levels(&day.stats, step))
                .collect::<Vec<_>>();
            let maxima = grouped
                .iter()
                .map(|levels| {
                    let max_side = levels
                        .values()
                        .map(|level| level.bid.max(level.ask))
                        .fold(0.0_f64, f64::max);
                    let max_volume = levels
                        .values()
                        .map(|level| level.volume())
                        .fold(0.0_f64, f64::max);
                    (max_side, max_volume)
                })
                .collect::<Vec<_>>();

            for (row_index, price_units) in rows.iter().enumerate() {
                let y = table_top + 34.0 + row_index as f32 * 15.0;
                for day_index in 0..DAYS {
                    let left = width * day_index as f32;
                    draw_level_row(
                        frame,
                        left,
                        width,
                        y,
                        *price_units,
                        grouped[day_index].get(price_units).copied(),
                        maxima[day_index],
                        fg,
                        muted,
                        buy,
                        sell,
                        warning,
                    );
                }
            }
        });
        vec![geometry]
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_day_metrics(
    frame: &mut canvas::Frame,
    day: &DisplayDay,
    left: f32,
    right: f32,
    top: f32,
    row_h: f32,
    fg: Color,
    muted: Color,
    buy: Color,
    sell: Color,
    accent: Color,
) {
    let stats = &day.stats;
    let mid = (left + right) * 0.5;
    let value_x = mid - 4.0;
    let right_value_x = right;
    let price = |value: Option<f64>| value.map_or_else(|| "—".to_string(), format_price);
    let metric = |frame: &mut canvas::Frame, label: &str, value: String, y: f32, color: Color| {
        draw_text(
            frame,
            label,
            Point::new(left, y),
            8.5,
            muted,
            Alignment::Start,
        );
        draw_text(
            frame,
            &value,
            Point::new(value_x, y),
            8.5,
            color,
            Alignment::End,
        );
    };

    metric(frame, "Open", price(stats.first.map(|(_, p)| p)), top, fg);
    draw_text(
        frame,
        "High",
        Point::new(mid + 5.0, top),
        8.5,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &price(stats.high),
        Point::new(right_value_x, top),
        8.5,
        fg,
        Alignment::End,
    );
    metric(frame, "Low", price(stats.low), top + row_h, fg);
    draw_text(
        frame,
        "Close",
        Point::new(mid + 5.0, top + row_h),
        8.5,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &price(stats.last.map(|(_, p)| p)),
        Point::new(right_value_x, top + row_h),
        8.5,
        fg,
        Alignment::End,
    );

    metric(
        frame,
        "Volume",
        abbr_large_numbers(stats.volume()),
        top + row_h * 2.0,
        fg,
    );
    draw_text(
        frame,
        "Notional",
        Point::new(mid + 5.0, top + row_h * 2.0),
        8.5,
        muted,
        Alignment::Start,
    );
    draw_text(
        frame,
        &format!("${}", abbr_large_numbers(stats.notional)),
        Point::new(right_value_x, top + row_h * 2.0),
        8.5,
        fg,
        Alignment::End,
    );

    let delta = stats.delta();
    let delta_pct = if stats.volume() > 0.0 {
        delta / stats.volume() * 100.0
    } else {
        0.0
    };
    metric(
        frame,
        "Delta",
        format!("{} ({delta_pct:+.1}%)", signed(delta)),
        top + row_h * 3.0,
        if delta >= 0.0 { buy } else { sell },
    );

    let (cvd_low, cvd_high) = stats.cvd_range();
    metric(
        frame,
        "CVD range",
        format!("L {}", signed(cvd_low)),
        top + row_h * 4.0,
        sell,
    );
    draw_text(
        frame,
        &format!("H {}", signed(cvd_high)),
        Point::new(right_value_x, top + row_h * 4.0),
        8.5,
        buy,
        Alignment::End,
    );
    if cvd_high > cvd_low {
        let line_left = left + 64.0;
        let line_right = right - 54.0;
        let y = top + row_h * 4.0;
        frame.fill_rectangle(
            Point::new(line_left, y),
            Size::new((line_right - line_left).max(0.0), 1.0),
            muted.scale_alpha(0.5),
        );
        let ratio = ((delta - cvd_low) / (cvd_high - cvd_low)).clamp(0.0, 1.0) as f32;
        frame.fill_rectangle(
            Point::new(line_left + (line_right - line_left) * ratio - 2.0, y - 2.0),
            Size::new(4.0, 4.0),
            accent,
        );
    }

    metric(
        frame,
        "OI Δ",
        day.oi_delta.map_or_else(|| "n/a".to_string(), signed),
        top + row_h * 5.0,
        day.oi_delta
            .map_or(muted, |value| if value >= 0.0 { buy } else { sell }),
    );

    for (offset, positive, label) in [
        (6.0, true, "Largest +Δ (5m)"),
        (7.0, false, "Largest -Δ (5m)"),
    ] {
        let result = stats.largest_bucket(positive);
        metric(
            frame,
            label,
            result.map_or_else(|| "—".to_string(), |(_, value)| signed(value)),
            top + row_h * offset,
            if positive { buy } else { sell },
        );
        if let Some((time, _)) = result {
            draw_text(
                frame,
                &format_time(time),
                Point::new(right_value_x, top + row_h * offset),
                8.0,
                muted,
                Alignment::End,
            );
        }
    }

    let trade = stats.largest_trade;
    metric(
        frame,
        "Largest trade",
        trade.map_or_else(
            || "—".to_string(),
            |trade| {
                format!(
                    "{} {} · ${}",
                    if trade.is_sell { "Sell" } else { "Buy" },
                    abbr_large_numbers(trade.qty),
                    abbr_large_numbers(trade.notional)
                )
            },
        ),
        top + row_h * 8.0,
        trade.map_or(muted, |trade| if trade.is_sell { sell } else { buy }),
    );
    if let Some(trade) = trade {
        draw_text(
            frame,
            &format!(
                "{} · {}",
                format_price(trade.price),
                format_time(trade.time.as_u64())
            ),
            Point::new(right_value_x, top + row_h * 8.0),
            7.5,
            muted,
            Alignment::End,
        );
    }
}

fn draw_table_header(frame: &mut canvas::Frame, left: f32, width: f32, y: f32, color: Color) {
    for (label, ratio, align) in [
        ("Bid", 0.19, Alignment::Center),
        ("Price", 0.39, Alignment::Center),
        ("Ask", 0.57, Alignment::Center),
        ("Delta", 0.75, Alignment::Center),
        ("Volume", 0.97, Alignment::End),
    ] {
        draw_text(
            frame,
            label,
            Point::new(left + width * ratio, y),
            8.0,
            color,
            align,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_level_row(
    frame: &mut canvas::Frame,
    left: f32,
    width: f32,
    y: f32,
    price_units: i64,
    level: Option<LevelStats>,
    maxima: (f64, f64),
    fg: Color,
    muted: Color,
    buy: Color,
    sell: Color,
    warning: Color,
) {
    let Some(level) = level else {
        draw_text(
            frame,
            &format_price(Price::from_units(price_units).to_f64()),
            Point::new(left + width * 0.39, y),
            8.0,
            muted,
            Alignment::Center,
        );
        return;
    };
    let side_max = maxima.0.max(f64::EPSILON);
    let cell_w = width * 0.16;
    frame.fill_rectangle(
        Point::new(left + width * 0.11, y - 6.0),
        Size::new(cell_w * (level.bid / side_max) as f32, 12.0),
        sell.scale_alpha(0.28),
    );
    frame.fill_rectangle(
        Point::new(left + width * 0.49, y - 6.0),
        Size::new(cell_w * (level.ask / side_max) as f32, 12.0),
        buy.scale_alpha(0.28),
    );
    if maxima.1 > 0.0 && (level.volume() - maxima.1).abs() <= f64::EPSILON {
        frame.fill_rectangle(
            Point::new(left + width * 0.82, y - 6.0),
            Size::new(width * 0.17, 12.0),
            warning.scale_alpha(0.28),
        );
    }
    draw_text(
        frame,
        &abbr_large_numbers(level.bid),
        Point::new(left + width * 0.27, y),
        8.0,
        sell,
        Alignment::End,
    );
    draw_text(
        frame,
        &format_price(Price::from_units(price_units).to_f64()),
        Point::new(left + width * 0.39, y),
        8.0,
        fg,
        Alignment::Center,
    );
    draw_text(
        frame,
        &abbr_large_numbers(level.ask),
        Point::new(left + width * 0.65, y),
        8.0,
        buy,
        Alignment::End,
    );
    let delta = level.delta();
    draw_text(
        frame,
        &signed(delta),
        Point::new(left + width * 0.80, y),
        8.0,
        if delta >= 0.0 { buy } else { sell },
        Alignment::End,
    );
    draw_text(
        frame,
        &abbr_large_numbers(level.volume()),
        Point::new(left + width * 0.98, y),
        8.0,
        fg,
        Alignment::End,
    );
}

fn shared_price_rows(
    days: &[DisplayDay; DAYS],
    min_tick: PriceStep,
    max_rows: usize,
) -> (PriceStep, Vec<i64>) {
    let high = days
        .iter()
        .filter_map(|day| day.stats.high)
        .fold(None, |acc: Option<f64>, value| {
            Some(acc.map_or(value, |current| current.max(value)))
        });
    let low = days
        .iter()
        .filter_map(|day| day.stats.low)
        .fold(None, |acc: Option<f64>, value| {
            Some(acc.map_or(value, |current| current.min(value)))
        });
    let (Some(high), Some(low)) = (high, low) else {
        return (min_tick, Vec::new());
    };
    let raw_step = ((high - low) / max_rows.max(1) as f64).max(min_tick.to_f64_lossy());
    let nice = nice_step(raw_step).max(min_tick.to_f64_lossy());
    let step = PriceStep {
        units: Price::from_f64(nice).units.max(min_tick.units),
    };
    let low_units = Price::from_f64(low).units.div_euclid(step.units) * step.units;
    let high_units = Price::from_f64(high).units.div_euclid(step.units) * step.units;
    let mut rows = Vec::new();
    let mut current = high_units;
    while current >= low_units && rows.len() < max_rows.saturating_add(1) {
        rows.push(current);
        let Some(next) = current.checked_sub(step.units) else {
            break;
        };
        current = next;
    }
    (step, rows)
}

fn group_levels(stats: &DayStats, step: PriceStep) -> BTreeMap<i64, LevelStats> {
    let mut grouped = BTreeMap::new();
    for (price, level) in &stats.levels {
        let bucket = price.div_euclid(step.units) * step.units;
        grouped
            .entry(bucket)
            .or_insert_with(LevelStats::default)
            .merge(*level);
    }
    grouped
}

fn nice_step(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let magnitude = 10.0_f64.powf(value.log10().floor());
    let normalized = value / magnitude;
    let factor = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    factor * magnitude
}

fn day_start(time: UnixMs) -> u64 {
    time.as_u64() / DAY_MS * DAY_MS
}

fn format_day(time: u64) -> String {
    chrono::DateTime::from_timestamp_millis(time as i64)
        .map(|date| date.format("%a, %d %b · UTC").to_string())
        .unwrap_or_else(|| "UTC".to_string())
}

fn format_time(time: u64) -> String {
    chrono::DateTime::from_timestamp_millis(time as i64)
        .map(|date| date.format("%H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn format_price(value: f64) -> String {
    if value.abs() >= 1_000.0 {
        format!("{value:.0}")
    } else if value.abs() >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.6}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn format_price_step(step: PriceStep) -> String {
    format!("${}", format_price(step.to_f64_lossy()))
}

fn signed(value: f64) -> String {
    if value >= 0.0 {
        format!("+{}", abbr_large_numbers(value))
    } else {
        format!("-{}", abbr_large_numbers(value.abs()))
    }
}

fn draw_text(
    frame: &mut canvas::Frame,
    text: &str,
    position: Point,
    size: f32,
    color: Color,
    alignment: Alignment,
) {
    frame.fill_text(canvas::Text {
        content: text.to_string(),
        position,
        size: iced::Pixels(size),
        color,
        align_x: alignment.into(),
        align_y: Alignment::Center.into(),
        font: style::AZERET_MONO,
        ..canvas::Text::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{adapter::Exchange, unit::Qty};

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    #[test]
    fn daily_stats_match_reference_semantics() {
        let mut stats = DayStats::default();
        stats.insert_trade(trade(1_000, 100.0, 2.0, false));
        stats.insert_trade(trade(2_000, 90.0, 1.0, true));
        assert_eq!(stats.first.map(|(_, price)| price), Some(100.0));
        assert_eq!(stats.last.map(|(_, price)| price), Some(90.0));
        assert_eq!(stats.high, Some(100.0));
        assert_eq!(stats.low, Some(90.0));
        assert_eq!(stats.volume(), 3.0);
        assert_eq!(stats.delta(), 1.0);
        assert_eq!(stats.notional, 290.0);
    }

    #[test]
    fn source_history_keeps_venues_independent_until_display_merge() {
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
        let now = UnixMs::now();
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.on_source_trades(binance, &[trade(now.as_u64(), 100.0, 2.0, false)], false);
        indicator.on_source_trades(bybit, &[trade(now.as_u64(), 100.0, 1.0, true)], false);
        let days = indicator.display_days(now);
        assert_eq!(days[0].stats.volume(), 3.0);
        assert_eq!(days[0].stats.delta(), 1.0);
    }
}
