//! Execution-derived reversal evidence. Past-only flow and volatility scales
//! are independent of canvas zoom, candle duration and footprint row size.
//! No raw executions, depth, HTTP clients or widgets are retained here.

use exchange::{
    Trade,
    unit::{Price, PriceStep},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const HISTORY_MS: u64 = 4 * 24 * 60 * 60 * 1_000;
pub const WARMUP_MS: u64 = 5 * 60 * 1_000;
pub const RETENTION_MS: u64 = HISTORY_MS + 2 * WARMUP_MS;
/// Replay streams older seconds through the detector; the live summary book
/// keeps its existing one-day bound independently of the displayed signal span.
pub const TAPE_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000 + 2 * WARMUP_MS;
pub const MAX_LEVELS_PER_SECOND: usize = 16;
pub const MAX_EVENTS: usize = 2_048;
pub const BREAK_WINDOW_MS: u64 = 5 * 60 * 1_000;
pub const POLICY_VERSION: &str = "adaptive-v1";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub absorption: bool,
    pub exhaustion: bool,
    pub show_observed: bool,
    /// Legacy tuning fields remain readable for saved-layout compatibility.
    /// Automatic analysis ignores them; only presentation switches are retained.
    pub band_ticks: u16,
    pub window_seconds: u16,
    pub min_absorption_usd: f64,
    pub sensitivity: f64,
    pub rejection_bands: f64,
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
        let mut second = Second::new(trade, tick());
        second.insert(trade, Detector::new(Config::default(), tick()).step(), true);
        second
    }
    fn approaching() -> Detector {
        let mut detector = Detector::new(Config::default(), tick());
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
            detector.process(&second(t, 1_036.0, 100_000.0, false));
        }
        detector.process(&second(363, 1_038.0, 70_000.0, false));
        detector.process(&second(364, 1_040.0, 20_000.0, false));
        for t in 365..367 {
            detector.process(&second(t, 1_040.0, 100.0, false));
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
                .all(|event| event.kind != Kind::Exhaustion),
            "one opposing second cannot confirm exhaustion"
        );
        detector.process(&second(368, 1_030.0, 100.0, true));
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
        assert!(base.seconds.len() <= (TAPE_RETENTION_MS / 1_000 + 1) as usize);
        assert!(base.estimated_bytes() < 10_000_000);
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
        let compacted = &base.seconds[&30_000_000];
        assert!(compacted.levels.len() <= MAX_LEVELS_PER_SECOND);
        assert!(compacted.complete);
        assert!(
            (compacted.levels.iter().map(|(_, v)| v.total()).sum::<f64>()
                - compacted.volume.total())
            .abs()
                < 0.01,
            "compaction preserves every execution's volume"
        );
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
        assert_eq!(config.band_ticks, Config::default().band_ticks);
        assert_eq!(config.window_seconds, Config::default().window_seconds);
        let legacy: crate::chart::kline::Config = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.orderflow, Config::default());
    }
    #[test]
    fn four_day_signals_outlive_the_bounded_summary_book_then_expire() {
        assert_eq!(HISTORY_MS, 4 * 24 * 3_600_000);
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
                time: UnixMs::new(TAPE_RETENTION_MS + 1_000),
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
        detector.process(&second(3 * 24 * 3_600, 1_025.0, 100.0, true));
        assert_eq!(detector.events()[0].observed, mark);
        detector.process(&second(
            (mark + RETENTION_MS) / 1_000 + 1,
            1_025.0,
            100.0,
            true,
        ));
        assert!(
            detector.events().is_empty(),
            "old signals expire at the four-day retention boundary"
        );
    }

    fn replay_fixture(
        config: Config,
        price_scale: f64,
        volume_scale: f64,
        mirror: bool,
    ) -> Detector {
        let min_tick = PriceStep {
            units: (tick().units as f64 * price_scale) as i64,
        };
        let mut tape = Tape::default();
        for t in 0..=368 {
            let (price, usd, sell) = match t {
                0..300 => (1_000.0, 100.0, t % 2 == 0),
                300..360 => (1_000.0 + (t - 300) as f64 * 0.6, 100.0, false),
                360..363 => (1_036.0, 100_000.0, false),
                363 => (1_038.0, 70_000.0, false),
                364 => (1_040.0, 20_000.0, false),
                365..367 => (1_040.0, 100.0, false),
                _ => (1_030.0, 100.0, true),
            };
            tape.insert(
                &[Trade {
                    time: UnixMs::new(t * 1_000),
                    price: Price::from_f64(if mirror {
                        (2_000.0 - price) * price_scale
                    } else {
                        price * price_scale
                    }),
                    qty: Qty::from_f64(usd * volume_scale),
                    is_sell: sell ^ mirror,
                }],
                min_tick,
                true,
            );
        }
        let mut detector = Detector::new(config, min_tick);
        for second in tape.seconds.values() {
            detector.process(second);
        }
        assert!(
            detector.events.iter().any(|e| e.kind == Kind::Exhaustion),
            "fixture must exercise a real confirmation"
        );
        detector
    }

    #[test]
    fn automatic_detection_is_identical_after_price_or_activity_rescaling() {
        let reference = replay_fixture(Config::default(), 1.0, 1.0, false);
        for (price_scale, volume_scale) in [(0.1, 1.0), (10.0, 1.0), (1.0, 0.01), (1.0, 100.0)] {
            let scaled = replay_fixture(Config::default(), price_scale, volume_scale, false);
            assert_eq!(reference.events.len(), scaled.events.len());
            for (a, b) in reference.events.iter().zip(&scaled.events) {
                assert_eq!(
                    (a.kind, a.bullish, a.observed, a.confirmed, a.broken),
                    (b.kind, b.bullish, b.observed, b.confirmed, b.broken)
                );
                assert!((b.price.to_f64() / price_scale - a.price.to_f64()).abs() < 1e-8);
                assert!((b.band_width / price_scale - a.band_width).abs() < 1e-8);
                assert!((b.pace_ratio - a.pace_ratio).abs() < 1e-10);
                assert!((b.threshold_usd / volume_scale - a.threshold_usd).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn upside_and_downside_pushes_use_the_same_automatic_rules() {
        let up = replay_fixture(Config::default(), 1.0, 1.0, false);
        let down = replay_fixture(Config::default(), 1.0, 1.0, true);
        assert_eq!(up.events.len(), down.events.len());
        for (a, b) in up.events.iter().zip(&down.events) {
            assert_eq!(
                (a.kind, a.observed, a.confirmed),
                (b.kind, b.observed, b.confirmed)
            );
            assert_ne!(a.bullish, b.bullish);
            assert!((a.price.to_f64() + b.price.to_f64() - 2_000.0).abs() < 1e-8);
            assert_eq!(a.band_width, b.band_width);
        }
    }

    #[test]
    fn legacy_knobs_and_visibility_switches_do_not_change_computed_signals() {
        let automatic = replay_fixture(Config::default(), 1.0, 1.0, false);
        let custom = replay_fixture(
            Config {
                absorption: false,
                exhaustion: false,
                show_observed: true,
                band_ticks: 500,
                window_seconds: 60,
                min_absorption_usd: 20_000_000.0,
                sensitivity: 5.0,
                rejection_bands: 6.0,
            },
            1.0,
            1.0,
            false,
        );
        assert_eq!(automatic.events.len(), custom.events.len());
        for (a, b) in automatic.events.iter().zip(&custom.events) {
            assert_eq!(
                (a.kind, a.bullish, a.observed, a.confirmed, a.price),
                (b.kind, b.bullish, b.observed, b.confirmed, b.price)
            );
            assert_eq!(a.threshold_usd, b.threshold_usd);
        }
    }

    #[test]
    fn resumed_aggression_cannot_confirm_exhaustion_despite_a_small_price_bounce() {
        let mut detector = approaching();
        for t in 360..363 {
            detector.process(&second(t, 1_036.0, 100_000.0, false));
        }
        detector.process(&second(363, 1_038.0, 70_000.0, false));
        detector.process(&second(364, 1_040.0, 20_000.0, false));
        detector.process(&second(365, 1_040.0, 100.0, false));
        detector.process(&second(366, 1_040.0, 100.0, false));
        assert!(detector.pending.iter().any(|e| e.kind == Kind::Exhaustion));
        detector.process(&second(367, 1_030.0, 1_000.0, true));
        let mut renewed = second(368, 1_030.0, 300_000.0, true);
        renewed.insert(
            Trade {
                time: UnixMs::new(368_000),
                price: Price::from_f64(1_030.0),
                qty: Qty::from_f64(200_000.0),
                is_sell: false,
            },
            tick(),
            true,
        );
        detector.process(&renewed);
        assert!(detector.events.iter().all(|e| e.kind != Kind::Exhaustion));
    }

    #[test]
    fn new_market_conditions_do_not_recalculate_old_confirmed_evidence() {
        let mut detector = replay_fixture(Config::default(), 1.0, 1.0, false);
        let old = detector
            .events
            .iter()
            .find(|e| e.kind == Kind::Exhaustion)
            .unwrap()
            .clone();
        for t in 369..430 {
            detector.process(&second(t, 1_100.0, 20_000_000.0, t % 2 == 0));
        }
        let retained = detector
            .events
            .iter()
            .find(|e| e.observed == old.observed && e.kind == old.kind)
            .unwrap();
        assert_eq!(retained.confirmed, old.confirmed);
        assert_eq!(retained.threshold_usd, old.threshold_usd);
        assert_eq!(retained.band_width, old.band_width);
        assert_eq!(retained.pace_ratio, old.pace_ratio);
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
    pub fn normalized(self) -> Self {
        Self {
            absorption: self.absorption,
            exhaustion: self.exhaustion,
            show_observed: self.show_observed,
            ..Self::default()
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
    storage_step: PriceStep,
    first_time: u64,
    last_time: u64,
}

impl Second {
    fn new(trade: Trade, min_tick: PriceStep) -> Self {
        Self {
            time: trade.time.as_u64() / 1_000 * 1_000,
            first: trade.price,
            last: trade.price,
            high: trade.price,
            low: trade.price,
            volume: Sides::default(),
            levels: Vec::new(),
            complete: true,
            // Start at exchange precision, coarsening this second only when
            // needed to retain ALL executed volume inside the 16-band cap.
            storage_step: min_tick,
            first_time: trade.time.as_u64(),
            last_time: trade.time.as_u64(),
        }
    }
    fn insert(&mut self, trade: Trade, _min_tick: PriceStep, qty_is_quote: bool) {
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
        loop {
            let price = trade.price.round_to_step(self.storage_step);
            match self
                .levels
                .binary_search_by_key(&price, |(price, _)| *price)
            {
                Ok(index) => {
                    self.levels[index].1.add(volume);
                    break;
                }
                Err(index) if self.levels.len() < MAX_LEVELS_PER_SECOND => {
                    self.levels.insert(index, (price, volume));
                    break;
                }
                Err(_) => {
                    let Some(units) = self.storage_step.units.checked_mul(2) else {
                        self.complete = false;
                        break;
                    };
                    self.storage_step.units = units;
                    // Sorted rounding is monotone: merge in place, without
                    // another allocation or dropping the overflowing print.
                    let mut kept = 0;
                    for index in 0..self.levels.len() {
                        let (price, volume) = self.levels[index];
                        let price = price.round_to_step(self.storage_step);
                        if kept > 0 && self.levels[kept - 1].0 == price {
                            self.levels[kept - 1].1.add(volume);
                        } else {
                            self.levels[kept] = (price, volume);
                            kept += 1;
                        }
                    }
                    self.levels.truncate(kept);
                }
            }
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
            let trade = Trade {
                price: trade.price.round_to_step(step),
                ..trade
            };
            let time = trade.time.as_u64() / 1_000 * 1_000;
            if closed.is_some_and(|closed| time <= closed) {
                continue;
            }
            self.seconds
                .entry(time)
                .or_insert_with(|| Second::new(trade, step))
                .insert(trade, step, qty_is_quote);
            self.prints += 1;
        }
        if let Some((&latest, _)) = self.seconds.last_key_value() {
            let oldest = latest.saturating_sub(TAPE_RETENTION_MS);
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
    /// Observation-time scale is frozen for confirmation, failure and replay risk.
    pub band_width: f64,
    pub activity_ratio: f64,
    pub reaction_activity_usd: f64,
    pub push_usd: f64,
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
    fn threshold(self) -> f64 {
        self.mean + 2.0 * self.variance.sqrt()
    }
}

fn inward_band(price: Price, extreme: Price, bullish: bool, step: PriceStep) -> Price {
    let distance = if bullish {
        price.units - extreme.units
    } else {
        extreme.units - price.units
    };
    // Coarse summary centers can lie just outside the exact extreme. Keep
    // their volume in the outer band, and include BOTH high/low boundary ties.
    Price::from_units(distance.max(0) / step.units * step.units)
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
    flow_baseline: [Baseline; 2],
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
                units: min_tick.units.max(1),
            },
            window: VecDeque::new(),
            context: VecDeque::new(),
            levels: BTreeMap::new(),
            volume: Sides::default(),
            baseline: [Baseline::default(); 2],
            flow_baseline: [Baseline::default(); 2],
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
        self.flow_baseline = [Baseline::default(); 2];
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
        // Only the most recent five minutes can acquire an outcome. Older
        // retained signals add no per-second work as the display span grows.
        let break_from = second.time.saturating_sub(BREAK_WINDOW_MS);
        for event in self
            .events
            .iter_mut()
            .rev()
            .take_while(|event| event.confirmed.is_some_and(|time| time >= break_from))
        {
            if event.broken.is_none()
                && event
                    .confirmed
                    .is_some_and(|t| t < second.time && second.time <= t + BREAK_WINDOW_MS)
            {
                let breach = if event.bullish {
                    second.low.units < event.price.units - Price::from_f64(event.band_width).units
                } else {
                    second.high.units > event.price.units + Price::from_f64(event.band_width).units
                };
                if breach {
                    event.broken = Some(now);
                }
            }
        }
        let mut retained = Vec::with_capacity(4);
        for mut event in self.pending.drain(..) {
            let breach = if event.bullish {
                second.low.units < event.price.units - Price::from_f64(event.band_width).units
            } else {
                second.high.units > event.price.units + Price::from_f64(event.band_width).units
            };
            if breach || now > event.observed + 45_000 {
                continue;
            }
            let reaction = if event.bullish {
                second.last.units - event.price.units
            } else {
                event.price.units - second.last.units
            };
            let mut reaction_flow = if event.bullish {
                second.volume.delta() > 0.0
            } else {
                second.volume.delta() < 0.0
            };
            if event.kind == Kind::Exhaustion {
                let mut flow = Sides::default();
                let mut opposing_seconds = 0;
                let mut flow_seconds = 0;
                for s in self
                    .window
                    .iter()
                    .chain(std::iter::once(second))
                    .filter(|s| s.time > event.observed && s.time + 3_000 > second.time)
                {
                    flow.add(s.volume);
                    flow_seconds += 1;
                    if (s.volume.delta() > 0.0) == event.bullish && s.volume.delta() != 0.0 {
                        opposing_seconds += 1;
                    }
                }
                let delta = if event.bullish {
                    flow.delta()
                } else {
                    -flow.delta()
                };
                let rate_scale = 3.0 / f64::from(flow_seconds.max(1));
                reaction_flow &= opposing_seconds >= 2
                    && delta > flow.total() * 0.15
                    && flow.side(!event.bullish) * rate_scale >= event.reaction_activity_usd
                    && flow.side(event.bullish) * rate_scale < event.push_usd * 0.50;
            }
            if now > event.observed
                && reaction >= Price::from_f64(event.rejection).units
                && reaction_flow
            {
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
        self.window.push_back(second.clone());
        self.context.push_back(second.clone());
        let window_ms = 15_000;
        while self
            .window
            .front()
            .is_some_and(|old| old.time + window_ms <= second.time)
        {
            let old = self.window.pop_front().expect("front exists");
            self.volume.subtract(old.volume);
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
        let context_range = context_high.units - context_low.units;
        // Use previous context to choose the price scale. Re-bin only the
        // bounded 15-second window. Exchange precision and summary resolution
        // are the only floors: neither BTC's price nor a fixed dollar grid is used.
        let past_high = self
            .context
            .iter()
            .rev()
            .skip(1)
            .map(|s| s.high)
            .max()
            .unwrap_or(second.high);
        let past_low = self
            .context
            .iter()
            .rev()
            .skip(1)
            .map(|s| s.low)
            .min()
            .unwrap_or(second.low);
        let summary_resolution = self
            .window
            .iter()
            .rev()
            .skip(1)
            .map(|s| s.storage_step.units)
            .fold(self.step.units, i64::max);
        let width = ((past_high.units - past_low.units) / 24).max(summary_resolution * 2);
        let analysis_step = PriceStep {
            units: (width / self.step.units).max(1) * self.step.units,
        };
        let band = analysis_step.to_f64_lossy();
        let rejection = Price::from_units(
            (analysis_step.units * 4).max((context_high.units - context_low.units) * 15 / 100),
        )
        .round_to_step(self.step)
        .to_f64();
        for bullish in [false, true] {
            let index = usize::from(bullish);
            let extreme = if bullish { low } else { high };
            // All three bands span their full width inside the traded range;
            // measure distance inward symmetrically from the exact high/low.
            self.levels.clear();
            for s in &self.window {
                for &(price, volume) in &s.levels {
                    let offset = inward_band(price, extreme, bullish, analysis_step);
                    self.levels.entry(offset).or_default().add(volume);
                }
            }
            let level = Price::from_units(0);
            let in_extreme = if bullish {
                second.last.units <= low.units + analysis_step.units
            } else {
                second.last.units >= high.units - analysis_step.units
            };
            let at_context_extreme = if bullish {
                low.units <= context_low.units + analysis_step.units
            } else {
                high.units >= context_high.units - analysis_step.units
            };
            let level_volume = self.levels.get(&level).copied().unwrap_or_default();
            let side = level_volume.side(bullish);
            let threshold = self.baseline[index].threshold();
            let prior_price = self
                .context
                .front()
                .map_or(second.last.units, |s| s.first.units);
            let approach = if bullish {
                prior_price - second.last.units
            } else {
                second.last.units - prior_price
            };
            let active_push = self.context.len() >= 60
                && approach >= (analysis_step.units * 4).max(context_range / 2);
            let cooldown = self.last_mark[index].is_some_and(|(time, previous)| {
                now < time + 60_000
                    || (now < time + 180_000
                        && (previous.units - extreme.units).abs() < analysis_step.units * 2)
            });
            let ready = self.baseline[index].samples >= WARMUP_MS / 1_000;
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
            let activity_threshold =
                self.flow_baseline[index].mean + 0.75 * self.flow_baseline[index].variance.sqrt();
            let strong_push = activity_threshold > 0.0 && earlier >= activity_threshold;
            let slowdown = strong_push && pace < 0.35;
            if ready
                && in_extreme
                && at_context_extreme
                && active_push
                && second.storage_step.to_f64_lossy() <= band
                && !cooldown
                && !self.pending.iter().any(|event| event.bullish == bullish)
            {
                let dominant = level_volume.total() > 0.0 && side / level_volume.total() >= 0.70;
                let dwell = self
                    .window
                    .iter()
                    .filter(|s| {
                        s.levels.iter().any(|(p, v)| {
                            inward_band(*p, extreme, bullish, analysis_step) == level
                                && v.side(bullish) > 0.0
                        })
                    })
                    .count();
                let absorption = threshold > 0.0
                    && side >= threshold
                    && dominant
                    && dwell >= 3
                    && side >= self.volume.side(bullish) * 0.25;
                let inward1 = level.add_steps(1, analysis_step);
                let inward2 = inward1.add_steps(1, analysis_step);
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
                let taper = side < v1 * 0.65 && v1 < v2;
                let exhaustion = slowdown && taper;
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
                        threshold_usd: if absorption {
                            threshold
                        } else {
                            activity_threshold
                        },
                        pace_ratio: pace,
                        rejection,
                        confirmation_price: None,
                        band_width: band,
                        activity_ratio: if activity_threshold > 0.0 {
                            earlier / activity_threshold
                        } else {
                            0.0
                        },
                        reaction_activity_usd: self.flow_baseline[1 - index].mean,
                        push_usd: earlier,
                    });
                }
            }
            let max_side = self
                .levels
                .values()
                .map(|volume| volume.side(bullish))
                .fold(0.0, f64::max);
            self.baseline[index].update(max_side);
            // Learn three-second flow directly instead of assuming independent
            // execution seconds: BTC activity arrives in correlated bursts.
            let three_second_flow = self
                .window
                .iter()
                .filter(|s| s.time + 3_000 > second.time)
                .map(|s| s.volume.side(bullish))
                .sum();
            self.flow_baseline[index].update(three_second_flow);
        }
    }
}
