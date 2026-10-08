//! Execution-derived reversal evidence. Analysis uses fixed seconds and price
//! bands, independent of canvas zoom, candle duration and footprint row size.
//! No raw executions, depth, HTTP clients or widgets are retained here.

use exchange::{
    Trade,
    unit::{Price, PriceStep},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const HISTORY_MS: u64 = 24 * 60 * 60 * 1_000;
pub const WARMUP_MS: u64 = 5 * 60 * 1_000;
pub const RETENTION_MS: u64 = HISTORY_MS + 2 * WARMUP_MS;
pub const MAX_LEVELS_PER_SECOND: usize = 32;
pub const MAX_EVENTS: usize = 512;
pub const BREAK_WINDOW_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub absorption: bool,
    pub exhaustion: bool,
    pub show_observed: bool,
    /// Analysis grid in venue minimum ticks, never the display tick multiplier.
    pub band_ticks: u16,
    pub window_seconds: u16,
    pub min_absorption_usd: f64,
    pub sensitivity: f64,
    pub rejection_bands: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Balanced,
    SelectiveAbsorption,
}

impl Preset {
    pub const ALL: [Self; 2] = [Self::Balanced, Self::SelectiveAbsorption];

    pub fn config(self) -> Config {
        match self {
            Self::Balanced => Config::default(),
            Self::SelectiveAbsorption => Config::selective(),
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Balanced => "Balanced",
            Self::SelectiveAbsorption => "Selective absorption",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        UnixMs,
        unit::{MinTicksize, Qty},
    };

    fn tick() -> PriceStep {
        MinTicksize::from(0.1).into()
    }
    fn second(time: u64, price: f64, usd: f64, sell: bool) -> Second {
        let trade = Trade {
            time: UnixMs::new(time * 1_000),
            price: Price::from_f64(price),
            qty: Qty::from_f64(usd),
            is_sell: sell,
        };
        let mut second = Second::new(trade);
        second.insert(trade, Detector::new(Config::default(), tick()).step(), true);
        second
    }
    fn approaching() -> Detector {
        let mut detector = Detector::new(Config::selective(), tick());
        for t in 0..300 {
            detector.process(&second(t, 1_000.0, 100.0, t % 2 == 0));
        }
        for t in 300..360 {
            detector.process(&second(t, 1_000.0 + (t - 300) as f64 * 0.6, 100.0, false));
        }
        detector
    }
    #[test]
    fn absorption_requires_later_rejection_and_opposing_flow_and_retains_failure() {
        let mut detector = approaching();
        for t in 360..364 {
            detector.process(&second(t, 1_040.0, 150_000.0, false));
        }
        assert!(detector.events().is_empty());
        assert!(
            detector
                .pending()
                .iter()
                .any(|event| event.kind == Kind::Absorption && !event.bullish)
        );
        detector.process(&second(364, 1_025.0, 100.0, false));
        assert!(
            detector.events().is_empty(),
            "price bounce alone cannot confirm"
        );
        detector.process(&second(365, 1_025.0, 100.0, true));
        assert_eq!(detector.events().len(), 1);
        let event = &detector.events()[0];
        assert!(event.confirmed.unwrap() > event.observed);
        assert_eq!(event.confirmation_price.unwrap(), Price::from_f64(1_025.0));
        detector.process(&second(366, 1_047.0, 100.0, false));
        assert_eq!(detector.events()[0].status(), Status::Broken);
        assert_eq!(detector.events()[0].confirmed, Some(365_999));
    }
    #[test]
    fn exhaustion_requires_volume_taper_and_slowing_pace() {
        let mut detector = approaching();
        detector.config.exhaustion = true;
        for t in 360..363 {
            detector.process(&second(t, 1_035.0, 100_000.0, false));
        }
        detector.process(&second(363, 1_040.0, 70_000.0, false));
        detector.process(&second(364, 1_045.0, 20_000.0, false));
        for t in 365..367 {
            detector.process(&second(t, 1_045.0, 100.0, false));
        }
        assert!(
            detector
                .pending()
                .iter()
                .any(|event| event.kind == Kind::Exhaustion)
        );
        detector.process(&second(367, 1_030.0, 100.0, true));
        assert!(
            detector
                .events()
                .iter()
                .any(|event| event.kind == Kind::Exhaustion)
        );
    }
    #[test]
    fn continuation_cancels_observation_without_backdating_or_repainting() {
        let mut detector = approaching();
        for t in 360..364 {
            detector.process(&second(t, 1_040.0, 150_000.0, false));
        }
        assert!(!detector.pending().is_empty());
        detector.process(&second(364, 1_050.0, 100.0, false));
        assert!(detector.events().is_empty());
        detector.process(&second(350, 1_020.0, 1_000_000.0, true));
        assert_eq!(detector.last_time(), Some(364_000));
    }

    #[test]
    fn much_later_revisit_does_not_reclassify_an_old_reversal() {
        let mut detector = approaching();
        for t in 360..364 {
            detector.process(&second(t, 1_040.0, 150_000.0, false));
        }
        detector.process(&second(364, 1_025.0, 100.0, true));
        assert_eq!(detector.events()[0].status(), Status::Confirmed);
        detector.process(&second(700, 1_060.0, 100.0, false));
        assert_eq!(detector.events()[0].status(), Status::Confirmed);
    }
    #[test]
    fn gaps_and_overflow_reset_warmup_without_reprocessing_old_seconds() {
        let mut detector = approaching();
        detector.reset_continuity();
        detector.process(&second(350, 1_040.0, 1_000_000.0, false));
        assert_eq!(detector.last_time(), Some(359_000));
        let mut overflow = second(360, 1_040.0, 1_000_000.0, false);
        overflow.complete = false;
        detector.process(&overflow);
        detector.process(&second(380, 1_040.0, 1_000_000.0, false));
        assert!(detector.pending().is_empty());
        assert!(detector.gap_resets >= 3);
    }
    #[test]
    fn tape_is_bounded_and_base_quote_units_agree() {
        let mut base = Tape::default();
        let mut quote = Tape::default();
        let trade = Trade {
            time: UnixMs::new(0),
            price: Price::from_f64(100_000.0),
            qty: Qty::from_f64(1.0),
            is_sell: true,
        };
        base.insert(&[trade], tick(), false);
        quote.insert(
            &[Trade {
                qty: Qty::from_f64(100_000.0),
                ..trade
            }],
            tick(),
            true,
        );
        assert_eq!(base.seconds[&0].volume.sell, quote.seconds[&0].volume.sell);
        for t in 0..30_000 {
            base.insert(
                &[Trade {
                    time: UnixMs::new(t * 1_000),
                    ..trade
                }],
                tick(),
                false,
            );
        }
        assert!(base.seconds.len() <= (RETENTION_MS / 1_000 + 1) as usize);
        assert!(base.estimated_bytes() < 8_000_000);
        for p in 0..100 {
            base.insert(
                &[Trade {
                    time: UnixMs::new(30_000_000),
                    price: Price::from_f64(100_000.0 + p as f64),
                    ..trade
                }],
                tick(),
                false,
            );
        }
        assert_eq!(
            base.seconds[&30_000_000].levels.len(),
            MAX_LEVELS_PER_SECOND
        );
        assert!(!base.seconds[&30_000_000].complete);
    }
    #[test]
    fn old_configuration_defaults_and_invalid_values_are_safe() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config, Config::default());
        let config = Config {
            min_absorption_usd: f64::NAN,
            band_ticks: 0,
            window_seconds: u16::MAX,
            ..config
        }
        .normalized();
        assert!(config.min_absorption_usd.is_finite());
        assert_eq!(config.band_ticks, 10);
        assert_eq!(config.window_seconds, 60);
        let legacy: crate::chart::kline::Config = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.orderflow, Config::default());
    }
    #[test]
    fn day_history_retains_old_summaries_and_marks_then_expires_them() {
        assert_eq!(HISTORY_MS, 24 * 3_600_000);
        let mut tape = Tape::default();
        let trade = Trade {
            time: UnixMs::new(0),
            price: Price::from_f64(100_000.0),
            qty: Qty::from_f64(1.0),
            is_sell: true,
        };
        tape.insert(&[trade], tick(), false);
        tape.insert(
            &[Trade {
                time: UnixMs::new(8 * 3_600_000),
                ..trade
            }],
            tick(),
            false,
        );
        assert!(
            tape.seconds.contains_key(&0),
            "history beyond four hours remains available"
        );
        tape.insert(
            &[Trade {
                time: UnixMs::new(RETENTION_MS + 1_000),
                ..trade
            }],
            tick(),
            false,
        );
        assert!(
            !tape.seconds.contains_key(&0),
            "old summaries do not accumulate indefinitely"
        );
        assert_eq!(tape.seconds.len(), 2);

        let mut detector = approaching();
        for t in 360..364 {
            detector.process(&second(t, 1_040.0, 150_000.0, false));
        }
        detector.process(&second(364, 1_025.0, 100.0, true));
        let mark = detector.events()[0].observed;
        detector.process(&second(8 * 3_600, 1_025.0, 100.0, true));
        assert_eq!(detector.events()[0].observed, mark);
        detector.process(&second(
            (mark + RETENTION_MS) / 1_000 + 1,
            1_025.0,
            100.0,
            true,
        ));
        assert!(
            detector.events().is_empty(),
            "old signals expire with the summary book"
        );
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            absorption: true,
            exhaustion: true,
            show_observed: false,
            band_ticks: 50,
            window_seconds: 15,
            min_absorption_usd: 1_000_000.0,
            sensitivity: 2.0,
            rejection_bands: 4.0,
        }
    }
}

impl Config {
    pub fn selective() -> Self {
        Self {
            exhaustion: false,
            min_absorption_usd: 250_000.0,
            rejection_bands: 3.0,
            ..Self::default()
        }
    }

    pub fn preset(self) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|preset| preset.config() == self)
    }

    pub fn normalized(self) -> Self {
        let defaults = Self::default();
        let finite = |value: f64, fallback: f64, low: f64, high: f64| {
            if value.is_finite() {
                value.clamp(low, high)
            } else {
                fallback
            }
        };
        Self {
            band_ticks: self.band_ticks.clamp(10, 500),
            window_seconds: self.window_seconds.clamp(5, 60),
            min_absorption_usd: finite(
                self.min_absorption_usd,
                defaults.min_absorption_usd,
                50_000.0,
                20_000_000.0,
            ),
            sensitivity: finite(self.sensitivity, defaults.sensitivity, 1.0, 5.0),
            rejection_bands: finite(self.rejection_bands, defaults.rejection_bands, 1.0, 6.0),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Sides {
    pub buy: f64,
    pub sell: f64,
}

impl Sides {
    pub fn total(self) -> f64 {
        self.buy + self.sell
    }
    pub fn delta(self) -> f64 {
        self.buy - self.sell
    }
    fn side(self, bullish: bool) -> f64 {
        if bullish { self.sell } else { self.buy }
    }
    fn add(&mut self, other: Self) {
        self.buy += other.buy;
        self.sell += other.sell;
    }
    fn subtract(&mut self, other: Self) {
        self.buy = (self.buy - other.buy).max(0.0);
        self.sell = (self.sell - other.sell).max(0.0);
    }
}

/// One second's sufficient statistics, preserving adapter order for timestamp ties.
#[derive(Debug, Clone)]
pub struct Second {
    pub time: u64,
    pub first: Price,
    pub last: Price,
    pub high: Price,
    pub low: Price,
    pub volume: Sides,
    pub levels: Vec<(Price, Sides)>,
    pub complete: bool,
    first_time: u64,
    last_time: u64,
}

impl Second {
    fn new(trade: Trade) -> Self {
        Self {
            time: trade.time.as_u64() / 1_000 * 1_000,
            first: trade.price,
            last: trade.price,
            high: trade.price,
            low: trade.price,
            volume: Sides::default(),
            levels: Vec::new(),
            complete: true,
            first_time: trade.time.as_u64(),
            last_time: trade.time.as_u64(),
        }
    }
    fn insert(&mut self, trade: Trade, step: PriceStep, qty_is_quote: bool) {
        let qty = trade.qty.to_f64();
        let notional = if qty_is_quote {
            qty
        } else {
            qty * trade.price.to_f64()
        };
        if !notional.is_finite() || notional <= 0.0 {
            return;
        }
        if trade.time.as_u64() < self.first_time {
            self.first_time = trade.time.as_u64();
            self.first = trade.price;
        }
        if trade.time.as_u64() >= self.last_time {
            self.last_time = trade.time.as_u64();
            self.last = trade.price;
        }
        self.high = self.high.max(trade.price);
        self.low = self.low.min(trade.price);
        let volume = if trade.is_sell {
            Sides {
                sell: notional,
                ..Sides::default()
            }
        } else {
            Sides {
                buy: notional,
                ..Sides::default()
            }
        };
        self.volume.add(volume);
        let price = trade.price.round_to_step(step);
        match self
            .levels
            .binary_search_by_key(&price, |(price, _)| *price)
        {
            Ok(index) => self.levels[index].1.add(volume),
            Err(index) if self.levels.len() < MAX_LEVELS_PER_SECOND => {
                self.levels.insert(index, (price, volume))
            }
            Err(_) => self.complete = false,
        }
    }
}

#[derive(Default, Debug)]
pub struct Tape {
    pub seconds: BTreeMap<u64, Second>,
    pub prints: u64,
}

impl Tape {
    pub fn insert(&mut self, trades: &[Trade], step: PriceStep, qty_is_quote: bool) {
        self.insert_after(trades, step, qty_is_quote, None);
    }
    /// Once a second is published it is immutable, including at the replay/live seam.
    pub fn insert_after(
        &mut self,
        trades: &[Trade],
        step: PriceStep,
        qty_is_quote: bool,
        closed: Option<u64>,
    ) {
        for &trade in trades {
            if trade.price.units <= 0
                || !trade.qty.to_f64().is_finite()
                || trade.qty.to_f64() <= 0.0
            {
                continue;
            }
            let time = trade.time.as_u64() / 1_000 * 1_000;
            if closed.is_some_and(|closed| time <= closed) {
                continue;
            }
            self.seconds
                .entry(time)
                .or_insert_with(|| Second::new(trade))
                .insert(trade, step, qty_is_quote);
            self.prints += 1;
        }
        if let Some((&latest, _)) = self.seconds.last_key_value() {
            let oldest = latest.saturating_sub(RETENTION_MS);
            while self
                .seconds
                .first_key_value()
                .is_some_and(|(&time, _)| time < oldest)
            {
                self.seconds.pop_first();
            }
        }
    }
    pub fn estimated_bytes(&self) -> usize {
        self.seconds
            .values()
            .map(|second| {
                std::mem::size_of::<Second>()
                    + 64
                    + second.levels.capacity() * std::mem::size_of::<(Price, Sides)>()
            })
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Absorption,
    Exhaustion,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Observed,
    Confirmed,
    Broken,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: Kind,
    /// Expected reaction, not the aggressor's side. Bullish absorption consumes sells.
    pub bullish: bool,
    pub price: Price,
    pub observed: u64,
    pub confirmed: Option<u64>,
    pub broken: Option<u64>,
    pub buy_usd: f64,
    pub sell_usd: f64,
    pub threshold_usd: f64,
    pub pace_ratio: f64,
    pub rejection: f64,
    pub confirmation_price: Option<Price>,
}

impl Event {
    pub fn status(&self) -> Status {
        if self.broken.is_some() {
            Status::Broken
        } else if self.confirmed.is_some() {
            Status::Confirmed
        } else {
            Status::Observed
        }
    }
    pub fn label(&self) -> &'static str {
        match (self.kind, self.bullish) {
            (Kind::Absorption, true) => "Sellers absorbed",
            (Kind::Absorption, false) => "Buyers absorbed",
            (Kind::Exhaustion, true) => "Selling exhaustion",
            (Kind::Exhaustion, false) => "Buying exhaustion",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Baseline {
    mean: f64,
    variance: f64,
    samples: u64,
}

impl Baseline {
    fn update(&mut self, sample: f64) {
        self.samples += 1;
        let alpha = 2.0 / 301.0;
        if self.samples == 1 {
            self.mean = sample;
            return;
        }
        let delta = sample - self.mean;
        self.mean += alpha * delta;
        self.variance = (1.0 - alpha) * (self.variance + alpha * delta * delta);
    }
    fn threshold(self, config: Config) -> f64 {
        config
            .min_absorption_usd
            .max(self.mean + config.sensitivity * self.variance.sqrt())
    }
}

/// Bounded rolling detector. Its work is proportional to recently touched bands,
/// not all historical prints. Replay and live processing call the same method.
pub struct Detector {
    pub config: Config,
    step: PriceStep,
    window: VecDeque<Second>,
    context: VecDeque<Second>,
    levels: BTreeMap<Price, Sides>,
    volume: Sides,
    baseline: [Baseline; 2],
    pending: Vec<Event>,
    events: VecDeque<Event>,
    last_time: Option<u64>,
    last_mark: [Option<(u64, Price)>; 2],
    pub evaluated_seconds: u64,
    pub gap_resets: u64,
}

impl Detector {
    pub fn new(config: Config, min_tick: PriceStep) -> Self {
        let config = config.normalized();
        Self {
            config,
            step: PriceStep {
                units: min_tick.units.max(1) * i64::from(config.band_ticks),
            },
            window: VecDeque::new(),
            context: VecDeque::new(),
            levels: BTreeMap::new(),
            volume: Sides::default(),
            baseline: [Baseline::default(); 2],
            pending: Vec::new(),
            events: VecDeque::new(),
            last_time: None,
            last_mark: [None; 2],
            evaluated_seconds: 0,
            gap_resets: 0,
        }
    }
    pub fn step(&self) -> PriceStep {
        self.step
    }
    pub fn events(&self) -> &VecDeque<Event> {
        &self.events
    }
    pub fn pending(&self) -> &[Event] {
        &self.pending
    }
    pub fn last_time(&self) -> Option<u64> {
        self.last_time
    }
    pub fn reset_continuity(&mut self) {
        self.window.clear();
        self.context.clear();
        self.levels.clear();
        self.volume = Sides::default();
        self.baseline = [Baseline::default(); 2];
        self.pending.clear();
        self.gap_resets += 1;
    }
    pub fn process(&mut self, second: &Second) {
        if self.last_time.is_some_and(|last| second.time <= last) {
            return;
        }
        // Short idle intervals occur even in BTC. Longer silence or an
        // overflowing second must not masquerade as an exhaustion signal.
        // Explicit transport interruptions also reset the live warm-up.
        if !second.complete
            || self
                .last_time
                .is_some_and(|last| second.time > last + 10_000)
        {
            self.reset_continuity();
            if !second.complete {
                self.last_time = Some(second.time);
                return;
            }
        }
        self.last_time = Some(second.time);
        self.evaluated_seconds += 1;
        let now = second.time + 999;
        let price = second.last.to_f64();
        let band = self.step.to_f64_lossy();
        for event in &mut self.events {
            if event.broken.is_none()
                && event
                    .confirmed
                    .is_some_and(|t| t < second.time && second.time <= t + BREAK_WINDOW_MS)
            {
                let breach = if event.bullish {
                    second.low.to_f64() < event.price.to_f64() - band
                } else {
                    second.high.to_f64() > event.price.to_f64() + band
                };
                if breach {
                    event.broken = Some(now);
                }
            }
        }
        let mut retained = Vec::with_capacity(4);
        for mut event in self.pending.drain(..) {
            let anchor = event.price.to_f64();
            let breach = if event.bullish {
                second.low.to_f64() < anchor - band
            } else {
                second.high.to_f64() > anchor + band
            };
            if breach || now > event.observed + 45_000 {
                continue;
            }
            let reaction = if event.bullish {
                price - anchor
            } else {
                anchor - price
            };
            let reaction_flow = if event.bullish {
                second.volume.delta() > 0.0
            } else {
                second.volume.delta() < 0.0
            };
            if now > event.observed && reaction >= event.rejection && reaction_flow {
                event.confirmed = Some(now);
                event.confirmation_price = Some(second.last);
                self.last_mark[usize::from(event.bullish)] = Some((now, event.price));
                self.events.push_back(event);
            } else {
                retained.push(event);
            }
        }
        self.pending = retained;
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        while self
            .events
            .front()
            .is_some_and(|event| event.observed < now.saturating_sub(RETENTION_MS))
        {
            self.events.pop_front();
        }

        self.volume.add(second.volume);
        for &(price, volume) in &second.levels {
            self.levels.entry(price).or_default().add(volume);
        }
        self.window.push_back(second.clone());
        self.context.push_back(second.clone());
        let window_ms = u64::from(self.config.window_seconds) * 1_000;
        while self
            .window
            .front()
            .is_some_and(|old| old.time + window_ms <= second.time)
        {
            let old = self.window.pop_front().expect("front exists");
            self.volume.subtract(old.volume);
            for (price, volume) in old.levels {
                if let Some(total) = self.levels.get_mut(&price) {
                    total.subtract(volume);
                    if total.total() < 0.01 {
                        self.levels.remove(&price);
                    }
                }
            }
        }
        while self
            .context
            .front()
            .is_some_and(|old| old.time + 180_000 <= second.time)
        {
            self.context.pop_front();
        }
        let high = self
            .window
            .iter()
            .map(|s| s.high)
            .max()
            .unwrap_or(second.high);
        let low = self
            .window
            .iter()
            .map(|s| s.low)
            .min()
            .unwrap_or(second.low);
        let context_high = self.context.iter().map(|s| s.high).max().unwrap_or(high);
        let context_low = self.context.iter().map(|s| s.low).min().unwrap_or(low);
        let context_range = context_high.to_f64() - context_low.to_f64();
        let rejection = (band * self.config.rejection_bands).max(context_range * 0.15);
        for bullish in [false, true] {
            let index = usize::from(bullish);
            let extreme = if bullish { low } else { high };
            let level = extreme.round_to_step(self.step);
            let in_extreme = if bullish {
                price <= low.to_f64() + band
            } else {
                price >= high.to_f64() - band
            };
            let at_context_extreme = if bullish {
                low.to_f64() <= context_low.to_f64() + band
            } else {
                high.to_f64() >= context_high.to_f64() - band
            };
            let level_volume = self.levels.get(&level).copied().unwrap_or_default();
            let side = level_volume.side(bullish);
            let threshold = self.baseline[index].threshold(self.config);
            let prior_price = self.context.front().map_or(price, |s| s.first.to_f64());
            let approach = if bullish {
                prior_price - price
            } else {
                price - prior_price
            };
            let active_push =
                self.context.len() >= 60 && approach >= (band * 4.0).max(context_range * 0.5);
            let cooldown = self.last_mark[index].is_some_and(|(time, previous)| {
                now < time + 60_000
                    || (now < time + 180_000
                        && (previous.to_f64() - level.to_f64()).abs() < band * 2.0)
            });
            let ready = self.baseline[index].samples >= WARMUP_MS / 1_000;
            if ready
                && in_extreme
                && at_context_extreme
                && active_push
                && !cooldown
                && !self.pending.iter().any(|event| event.bullish == bullish)
            {
                let dominant = level_volume.total() > 0.0 && side / level_volume.total() >= 0.70;
                let dwell = self
                    .window
                    .iter()
                    .filter(|s| {
                        s.levels
                            .iter()
                            .any(|(p, v)| *p == level && v.side(bullish) > 0.0)
                    })
                    .count();
                let absorption = self.config.absorption
                    && side >= threshold
                    && dominant
                    && dwell >= 3
                    && side >= self.volume.side(bullish) * 0.25;
                let mut recent = 0.0;
                let mut earlier = 0.0;
                for s in &self.window {
                    if s.time + 3_000 > second.time {
                        recent += s.volume.side(bullish);
                    } else if s.time + 6_000 > second.time {
                        earlier += s.volume.side(bullish);
                    }
                }
                let pace = if earlier > 0.0 { recent / earlier } else { 1.0 };
                let inward1 = level.add_steps(if bullish { 1 } else { -1 }, self.step);
                let inward2 = inward1.add_steps(if bullish { 1 } else { -1 }, self.step);
                let v1 = self
                    .levels
                    .get(&inward1)
                    .copied()
                    .unwrap_or_default()
                    .side(bullish);
                let v2 = self
                    .levels
                    .get(&inward2)
                    .copied()
                    .unwrap_or_default()
                    .side(bullish);
                let taper =
                    side < v1 * 0.65 && v1 < v2 && v2 >= self.config.min_absorption_usd * 0.20;
                let exhaustion = self.config.exhaustion
                    && earlier >= self.config.min_absorption_usd * 0.50
                    && pace < 0.35
                    && taper;
                if absorption || exhaustion {
                    self.pending.push(Event {
                        kind: if absorption {
                            Kind::Absorption
                        } else {
                            Kind::Exhaustion
                        },
                        bullish,
                        price: extreme,
                        observed: now,
                        confirmed: None,
                        broken: None,
                        buy_usd: level_volume.buy,
                        sell_usd: level_volume.sell,
                        threshold_usd: threshold,
                        pace_ratio: pace,
                        rejection,
                        confirmation_price: None,
                    });
                }
            }
            let max_side = self
                .levels
                .values()
                .map(|volume| volume.side(bullish))
                .fold(0.0, f64::max);
            self.baseline[index].update(max_side);
        }
    }
}
