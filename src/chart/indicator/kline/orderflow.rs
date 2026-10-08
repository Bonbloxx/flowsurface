use super::KlineIndicatorImpl;
use super::daily_delta::delta_history_colors;
use super::footprint_history::draw_text;
use crate::chart::{Message, ViewState};
use data::chart::{
    Basis, PlotData,
    kline::KlineDataPoint,
    orderflow::{self, Detector, Event, Kind, Status, Tape},
};
use exchange::{
    TickerInfo, Trade, UnixMs,
    adapter::{Exchange, MarketKind},
    unit::PriceStep,
};
use iced::{
    Alignment, Element, Point, Rectangle,
    theme::palette::Extended,
    widget::canvas::{self, Path, Stroke},
};
use std::cell::RefCell;

struct History {
    id: uuid::Uuid,
    from: UnixMs,
    to: UnixMs,
    tape: Tape,
    handle: Option<iced::task::Handle>,
    ingest_us: u128,
    max_chunk_us: u128,
}

pub struct OrderflowIndicator {
    source: Option<TickerInfo>,
    config: orderflow::Config,
    tape: Tape,
    detector: Option<Detector>,
    history: Option<History>,
    covered_from: Option<u64>,
    history_anchor: Option<(UnixMs, UnixMs)>,
    notice: Option<String>,
    hover: RefCell<Vec<(Point, f32, Event)>>,
}

impl Default for OrderflowIndicator {
    fn default() -> Self {
        Self {
            source: None,
            config: orderflow::Config::default(),
            tape: Tape::default(),
            detector: None,
            history: None,
            covered_from: None,
            history_anchor: None,
            notice: None,
            hover: RefCell::new(Vec::new()),
        }
    }
}

impl OrderflowIndicator {
    pub fn supports(source: TickerInfo) -> bool {
        source.exchange() == Exchange::BinanceLinear
            && source
                .ticker
                .to_full_symbol_and_type()
                .0
                .eq_ignore_ascii_case("BTCUSDT")
    }
    pub fn configure(&mut self, config: orderflow::Config, source: TickerInfo) {
        let config = config.normalized();
        let source_changed = self.source != Some(source);
        let grid_changed = self.config.band_ticks != config.band_ticks;
        let changed = self.config != config;
        self.source = Some(source);
        self.config = config;
        if source_changed || grid_changed {
            self.tape = Tape::default();
            self.history = None;
            self.covered_from = None;
            self.history_anchor = None;
            self.notice = None;
        }
        if changed || source_changed || self.detector.is_none() {
            self.rebuild();
        }
    }
    fn qty_is_quote(&self) -> bool {
        exchange::unit::qty::volume_size_unit() == exchange::SizeUnit::Quote
            || self
                .source
                .is_some_and(|source| source.market_type() == MarketKind::InversePerps)
    }
    fn rebuild(&mut self) {
        let Some(source) = self.source else {
            return;
        };
        let mut detector = Detector::new(self.config, source.min_ticksize.into());
        let latest = self
            .tape
            .seconds
            .last_key_value()
            .map_or(0, |(&time, _)| time);
        for second in self
            .tape
            .seconds
            .values()
            .filter(|second| second.time < latest.saturating_sub(1_000))
        {
            detector.process(second);
        }
        self.detector = Some(detector);
    }
    pub fn plan_history(&mut self, now: UnixMs) -> Option<(UnixMs, UnixMs)> {
        let source = self.source?;
        if !Self::supports(source) || self.history.is_some() || self.covered_from.is_some() {
            return None;
        }
        let (from, to) = *self.history_anchor.get_or_insert_with(|| {
            let to = UnixMs::new(now.as_u64() / 1_000 * 1_000).saturating_sub(1);
            (
                to.saturating_sub(orderflow::HISTORY_MS + orderflow::WARMUP_MS - 1),
                to,
            )
        });
        Some((from, to))
    }
    pub fn begin_history(&mut self, id: uuid::Uuid, from: UnixMs, to: UnixMs) {
        self.history = Some(History {
            id,
            from,
            to,
            tape: Tape::default(),
            handle: None,
            ingest_us: 0,
            max_chunk_us: 0,
        });
        self.notice = Some("Loading 4h execution history…".to_string());
    }
    pub fn owns(&self, id: uuid::Uuid) -> bool {
        self.history
            .as_ref()
            .is_some_and(|history| history.id == id)
    }
    pub fn request_id(&self) -> Option<uuid::Uuid> {
        self.history.as_ref().map(|history| history.id)
    }
    pub fn set_handle(&mut self, id: uuid::Uuid, handle: iced::task::Handle) {
        if let Some(history) = self.history.as_mut().filter(|history| history.id == id) {
            history.handle = Some(handle.abort_on_drop());
        }
    }
    pub fn stage(&mut self, id: uuid::Uuid, source: TickerInfo, trades: &[Trade]) {
        if self.source != Some(source) {
            return;
        }
        let qty_is_quote = self.qty_is_quote();
        let Some(step) = self.detector.as_ref().map(Detector::step) else {
            return;
        };
        if let Some(history) = self.history.as_mut().filter(|history| history.id == id) {
            // The connector owns the sequential paging cursor; this bounded
            // staging book is published only on terminal success.
            let started = std::time::Instant::now();
            history.tape.insert(trades, step, qty_is_quote);
            let us = started.elapsed().as_micros();
            history.ingest_us += us;
            history.max_chunk_us = history.max_chunk_us.max(us);
        }
    }
    pub fn finish(&mut self, id: uuid::Uuid, success: bool) -> bool {
        if !self.owns(id) {
            return false;
        }
        let history = self.history.take().expect("owned history");
        if !success {
            self.notice =
                Some("History incomplete; retrying. Live signals need warm-up.".to_string());
            return true;
        }
        let prints = history.tape.prints;
        let seconds = history.tape.seconds.len();
        let mut tape = history.tape;
        // Historical seconds replace their live overlap authoritatively. Keep
        // the current second and newer live tail, without re-counting prints.
        let tail = self
            .tape
            .seconds
            .split_off(&(history.to.as_u64() / 1_000 * 1_000 + 1_000));
        tape.seconds.extend(tail);
        tape.insert(
            &[],
            self.detector.as_ref().expect("configured detector").step(),
            true,
        );
        self.tape = tape;
        self.covered_from = Some(history.from.as_u64());
        self.notice = None;
        let started = std::time::Instant::now();
        self.rebuild();
        log::info!(
            "orderflow_history source=BinanceBTC from={} to={} prints={} seconds={} compact_bytes={} replay_ms={} ingest_us={} max_chunk_us={} marks={}",
            history.from,
            history.to,
            prints,
            seconds,
            self.tape.estimated_bytes(),
            started.elapsed().as_millis(),
            history.ingest_us,
            history.max_chunk_us,
            self.detector
                .as_ref()
                .map_or(0, |detector| detector.events().len())
        );
        true
    }
    pub fn insert_live(&mut self, source: TickerInfo, trades: &[Trade]) -> bool {
        if self.source != Some(source) || !Self::supports(source) {
            return false;
        }
        let qty_is_quote = self.qty_is_quote();
        let Some(detector) = self.detector.as_mut() else {
            return false;
        };
        let before = detector.last_time();
        // Small adapter batches may be reordered within a second; publish only
        // seconds at least two seconds behind the newest received timestamp.
        self.tape
            .insert_after(trades, detector.step(), qty_is_quote, detector.last_time());
        let latest = self
            .tape
            .seconds
            .last_key_value()
            .map_or(0, |(&time, _)| time);
        let from = detector.last_time().map_or(0, |time| time + 1);
        if from >= latest.saturating_sub(1_000) {
            return false;
        }
        for (_, second) in self.tape.seconds.range(from..latest.saturating_sub(1_000)) {
            detector.process(second);
        }
        before != detector.last_time()
    }
    pub fn continuity_lost(&mut self) {
        if let Some(detector) = self.detector.as_mut() {
            detector.reset_continuity();
        }
        self.history = None;
        self.covered_from = None;
        self.history_anchor = None;
    }
}

impl KlineIndicatorImpl for OrderflowIndicator {
    fn orderflow(&mut self) -> Option<&mut OrderflowIndicator> {
        Some(self)
    }
    fn clear_all_caches(&mut self) {
        self.hover.borrow_mut().clear();
    }
    fn clear_crosshair_caches(&mut self) {}
    fn element<'a>(
        &'a self,
        _chart: &'a ViewState,
        _labels: bool,
        _range: std::ops::RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        iced::widget::row![].into()
    }
    fn unavailable_message(&self, _chart: &ViewState, _indicator: &str) -> Option<String> {
        self.source
            .filter(|source| !Self::supports(*source))
            .map(|_| {
                "Absorption & Exhaustion currently supports Binance BTCUSDT perpetuals.".into()
            })
    }
    fn draw_overlay(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        _source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        _step: PriceStep,
    ) {
        let Some(detector) = self.detector.as_ref() else {
            return;
        };
        let Basis::Time(interval) = chart.basis else {
            return;
        };
        let scale = chart.scaling.max(0.01);
        let (buy, sell) = delta_history_colors(palette);
        let mut hover = self.hover.borrow_mut();
        hover.clear();
        for event in detector.events().iter().chain(
            detector
                .pending()
                .iter()
                .filter(|_| self.config.show_observed),
        ) {
            let radius = marker_radius(event) / scale;
            let time = event.confirmed.unwrap_or(event.observed);
            let x =
                chart.interval_to_x(time / interval.to_milliseconds() * interval.to_milliseconds());
            let y = chart.price_to_y(event.price);
            if x < region.x - radius
                || x > region.x + region.width + radius
                || y < region.y - radius
                || y > region.y + region.height + radius
            {
                continue;
            }
            let point = Point::new(x, y);
            let color = if event.broken.is_some() {
                palette.background.strong.text.scale_alpha(0.45)
            } else if event.bullish {
                buy
            } else {
                sell
            };
            let path = match event.kind {
                Kind::Absorption => Path::rectangle(
                    Point::new(x - radius, y - radius),
                    iced::Size::new(radius * 2.0, radius * 2.0),
                ),
                Kind::Exhaustion => Path::new(|builder| {
                    builder.move_to(Point::new(x, y - radius));
                    builder.line_to(Point::new(x + radius, y));
                    builder.line_to(Point::new(x, y + radius));
                    builder.line_to(Point::new(x - radius, y));
                    builder.close();
                }),
            };
            if event.status() != Status::Observed {
                frame.fill(&path, color.scale_alpha(0.85));
            }
            frame.stroke(
                &path,
                Stroke::default().with_color(color).with_width(1.8 / scale),
            );
            hover.push((point, radius, event.clone()));
        }
        let notice = self.notice.clone().or_else(|| {
            if self.covered_from.is_none() {
                Some("Orderflow: warming up / waiting for execution history".into())
            } else {
                None
            }
        });
        if let Some(notice) = notice {
            draw_text(
                frame,
                &notice,
                Point::new(region.x + 8.0 / scale, region.y + 16.0 / scale),
                10.0 / scale,
                palette.background.base.text,
                Alignment::Start,
            );
        }
    }
    fn draw_hover(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        _source: &PlotData<KlineDataPoint>,
        palette: &Extended,
        region: Rectangle,
        cursor: Point,
    ) {
        let scale = chart.scaling.max(0.01);
        let hover = self.hover.borrow();
        let Some((point, radius, event)) = hover
            .iter()
            .filter(|(point, radius, event)| {
                marker_contains(*point, *radius, event.kind, cursor, 3.0 / scale)
            })
            .min_by(|(a, _, _), (b, _, _)| a.distance(cursor).total_cmp(&b.distance(cursor)))
        else {
            return;
        };
        let labels = [
            format!("{} · {:?} · Binance BTC", event.label(), event.status()),
            format!(
                "Band buys ${:.0} / sells ${:.0} · {}s",
                event.buy_usd, event.sell_usd, self.config.window_seconds
            ),
            format!(
                "Threshold ${:.0} · pace {:.0}% · rejection ${:.1}",
                event.threshold_usd,
                event.pace_ratio * 100.0,
                event.rejection
            ),
            match event.kind {
                Kind::Absorption => format!(
                    "Size: {:.2}× threshold activity · intensity, not win probability",
                    absorption_ratio(event)
                ),
                Kind::Exhaustion => format!(
                    "Size: {:.0}% pace drop · intensity, not win probability",
                    (1.0 - event.pace_ratio) * 100.0
                ),
            },
            format!(
                "Observed {} · confirmed {} UTC",
                timestamp(event.observed),
                event.confirmed.map_or_else(|| "pending".into(), timestamp)
            ),
        ];
        let width = (470.0 / scale).min((region.width - 16.0 / scale).max(1.0));
        let height = 86.0 / scale;
        let offset = radius + 8.0 / scale;
        let x = point
            .x
            .clamp(region.x, (region.x + region.width - width).max(region.x));
        let y = if point.y - height - offset >= region.y {
            point.y - height - offset
        } else {
            (point.y + offset).min((region.y + region.height - height).max(region.y))
        };
        let panel = Path::rectangle(Point::new(x, y), iced::Size::new(width, height));
        frame.fill(&panel, palette.background.base.color.scale_alpha(0.97));
        frame.stroke(
            &panel,
            Stroke::default()
                .with_color(palette.background.strong.color)
                .with_width(1.0 / scale),
        );
        for (index, label) in labels.iter().enumerate() {
            draw_text(
                frame,
                label,
                Point::new(x + 8.0 / scale, y + (14.0 + index as f32 * 14.0) / scale),
                10.0 / scale,
                palette.background.base.text,
                Alignment::Start,
            );
        }
    }
}

fn absorption_ratio(event: &Event) -> f64 {
    let aggressive = if event.bullish {
        event.sell_usd
    } else {
        event.buy_usd
    };
    aggressive / event.threshold_usd.max(1.0)
}

/// Screen-pixel radius based only on evidence captured at observation. Later
/// confirmation/failure and new market activity cannot resize an existing mark.
fn marker_radius(event: &Event) -> f32 {
    let intensity = match event.kind {
        // 1× threshold is the minimum size; 4× and above reach the cap. Log
        // scaling keeps exceptional prints from obscuring the candle chart.
        Kind::Absorption => absorption_ratio(event).clamp(1.0, 4.0).log2() / 2.0,
        // Exhaustion measures the collapse in pace, not high volume at the
        // extreme. Its qualification boundary is a 65% pace drop.
        Kind::Exhaustion => (1.0 - event.pace_ratio / 0.35).clamp(0.0, 1.0),
    };
    9.0 + 5.0 * intensity as f32
}

fn marker_contains(point: Point, radius: f32, kind: Kind, cursor: Point, pad: f32) -> bool {
    let dx = (point.x - cursor.x).abs();
    let dy = (point.y - cursor.y).abs();
    match kind {
        Kind::Absorption => dx.max(dy) <= radius + pad,
        Kind::Exhaustion => dx + dy <= radius + pad * std::f32::consts::SQRT_2,
    }
}

fn timestamp(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map_or_else(|| "?".into(), |time| time.format("%H:%M:%S").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        Ticker,
        unit::{Price, Qty},
    };

    fn source(exchange: Exchange) -> TickerInfo {
        TickerInfo::new(Ticker::new("BTCUSDT", exchange), 0.1, 0.001, None)
    }
    fn trade(ms: u64, price: f64) -> Trade {
        Trade {
            time: UnixMs::new(ms),
            price: Price::from_f64(price),
            qty: Qty::from_f64(1.0),
            is_sell: false,
        }
    }
    fn event(kind: Kind) -> Event {
        Event {
            kind,
            bullish: true,
            price: Price::from_f64(100_000.0),
            observed: 1_000,
            confirmed: None,
            broken: None,
            buy_usd: 100_000.0,
            sell_usd: 1_000_000.0,
            threshold_usd: 1_000_000.0,
            pace_ratio: 0.35,
            rejection: 20.0,
            confirmation_price: None,
        }
    }
    #[test]
    fn absorption_size_is_relative_to_activity_and_frozen_after_observation() {
        let mut mark = event(Kind::Absorption);
        let minimum = marker_radius(&mark);
        mark.sell_usd *= 2.0;
        let larger = marker_radius(&mark);
        assert!(larger > minimum);
        mark.sell_usd *= 10.0;
        mark.threshold_usd *= 10.0;
        assert_eq!(marker_radius(&mark), larger, "same normalized evidence");
        mark.confirmed = Some(10_000);
        mark.broken = Some(20_000);
        assert_eq!(marker_radius(&mark), larger, "outcomes do not resize marks");
        mark.sell_usd *= 100.0;
        assert!(marker_radius(&mark) <= 14.0, "outliers stay bounded");
        mark.bullish = false;
        mark.buy_usd = mark.sell_usd;
        assert_eq!(marker_radius(&mark), 14.0, "same sizing for either side");
    }
    #[test]
    fn exhaustion_size_tracks_pace_collapse_not_band_volume() {
        let mut mark = event(Kind::Exhaustion);
        let minimum = marker_radius(&mark);
        mark.pace_ratio = 0.20;
        let larger = marker_radius(&mark);
        assert!(larger > minimum);
        mark.sell_usd *= 100.0;
        assert_eq!(marker_radius(&mark), larger);
        mark.pace_ratio = 0.0;
        assert_eq!(marker_radius(&mark), 14.0);
        assert!(marker_contains(
            Point::ORIGIN,
            14.0,
            Kind::Exhaustion,
            Point::new(13.5, 0.0),
            0.0
        ));
        assert!(!marker_contains(
            Point::ORIGIN,
            14.0,
            Kind::Exhaustion,
            Point::new(13.5, 13.5),
            0.0
        ));
        assert!(marker_contains(
            Point::ORIGIN,
            14.0,
            Kind::Absorption,
            Point::new(13.5, 13.5),
            0.0
        ));
    }
    #[test]
    fn four_hours_plus_warmup_are_requested_and_retry_keeps_identity_bounds() {
        let mut indicator = OrderflowIndicator::default();
        indicator.configure(
            orderflow::Config::default(),
            source(Exchange::BinanceLinear),
        );
        let (from, to) = indicator.plan_history(UnixMs::new(20_000_000)).unwrap();
        assert_eq!(
            to.as_u64() - from.as_u64() + 1,
            orderflow::HISTORY_MS + orderflow::WARMUP_MS
        );
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, from, to);
        assert!(indicator.plan_history(UnixMs::new(21_000_000)).is_none());
        assert!(indicator.finish(id, false));
        assert_eq!(
            indicator.plan_history(UnixMs::new(21_000_000)),
            Some((from, to))
        );
    }
    #[test]
    fn historical_overlap_replaces_live_once_and_partial_fetch_is_not_published() {
        let source = source(Exchange::BinanceLinear);
        let mut indicator = OrderflowIndicator::default();
        indicator.configure(orderflow::Config::default(), source);
        indicator.insert_live(
            source,
            &[
                trade(1_000, 100_000.0),
                trade(2_000, 100_000.0),
                trade(4_000, 100_000.0),
            ],
        );
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, UnixMs::new(0), UnixMs::new(2_999));
        indicator.stage(id, source, &[trade(1_000, 100_000.0)]);
        assert!(indicator.finish(id, false));
        assert!(indicator.covered_from.is_none());
        indicator.begin_history(id, UnixMs::new(0), UnixMs::new(2_999));
        indicator.stage(
            id,
            source,
            &[trade(1_000, 100_000.0), trade(2_000, 100_000.0)],
        );
        let staged = indicator.history.as_ref().unwrap().tape.seconds[&1_000]
            .volume
            .total();
        assert!(indicator.finish(id, true));
        assert_eq!(indicator.tape.seconds[&1_000].volume.total(), staged);
        assert!(indicator.tape.seconds.contains_key(&4_000));
        assert_eq!(indicator.covered_from, Some(0));
        assert!(!indicator.finish(id, true));
    }
    #[test]
    fn unsupported_venue_cannot_enter_a_binance_detector_and_grid_change_restarts_history() {
        let mut indicator = OrderflowIndicator::default();
        let binance = source(Exchange::BinanceLinear);
        indicator.configure(orderflow::Config::default(), binance);
        assert!(!indicator.insert_live(source(Exchange::BybitLinear), &[trade(1_000, 100_000.0)]));
        assert!(indicator.tape.seconds.is_empty());
        let (from, to) = indicator.plan_history(UnixMs::new(20_000_000)).unwrap();
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, from, to);
        indicator.configure(
            orderflow::Config {
                band_ticks: 100,
                ..orderflow::Config::default()
            },
            binance,
        );
        assert!(!indicator.owns(id));
        assert!(indicator.plan_history(UnixMs::new(21_000_000)).is_some());
        indicator.configure(orderflow::Config::default(), source(Exchange::BybitLinear));
        assert!(indicator.plan_history(UnixMs::new(21_000_000)).is_none());
    }

    #[test]
    fn queued_live_batches_after_replay_and_late_prints_do_not_invert_the_range() {
        let mut indicator = OrderflowIndicator::default();
        let source = source(Exchange::BinanceLinear);
        indicator.configure(orderflow::Config::default(), source);
        indicator.insert_live(
            source,
            &[
                trade(1_000, 100_000.0),
                trade(2_000, 100_000.0),
                trade(3_000, 100_000.0),
            ],
        );
        indicator.rebuild();
        let volume = indicator.tape.seconds[&1_000].volume.total();
        assert!(!indicator.insert_live(source, &[trade(3_001, 100_000.0)]));
        assert!(!indicator.insert_live(source, &[trade(1_000, 100_000.0)]));
        assert_eq!(
            indicator.tape.seconds[&1_000].volume.total(),
            volume,
            "published seconds are immutable"
        );
        assert!(indicator.insert_live(source, &[trade(4_000, 100_000.0)]));
        assert_eq!(
            indicator.detector.as_ref().unwrap().last_time(),
            Some(2_000)
        );
    }
}
