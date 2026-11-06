//! Bounded, timestamp-based execution pace. No raw trades or daily books are retained.
use exchange::{Trade, UnixMs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RETENTION_SECS: u64 = 4 * 60 * 60;
const CAPACITY: usize = RETENTION_SECS as usize + 660 + 1;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub window_secs: u16,
    pub baseline_secs: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window_secs: 15,
            baseline_secs: 120,
        }
    }
}

impl Config {
    pub fn normalized(self) -> Self {
        Self {
            window_secs: self.window_secs.clamp(1, 60),
            baseline_secs: self.baseline_secs.clamp(30, 600),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    second: u64,
    buy: u64,
    sell: u64,
    notional: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PacePoint {
    /// Exclusive end of the completed rolling window.
    pub time: UnixMs,
    pub buy: f32,
    pub sell: f32,
    pub count: u64,
    pub average_notional: f64,
    pub baseline: Option<f32>,
    pub complete: bool,
}

impl PacePoint {
    pub fn total(self) -> f32 {
        self.buy + self.sell
    }

    pub fn relative(self) -> Option<f32> {
        self.baseline
            .filter(|value| *value > 0.0)
            .map(|value| self.total() / value)
    }
}

pub struct TradePace {
    config: Config,
    buckets: Box<[Bucket]>,
    points: BTreeMap<UnixMs, PacePoint>,
    /// Complete-second intervals. Closed segments survive reconnects and setting changes.
    coverage: BTreeMap<u64, Option<u64>>,
    connected: bool,
    accept_from: u64,
    last_publish: Option<u64>,
    dirty: Option<(u64, u64)>,
}

impl TradePace {
    pub fn new(config: Config, now: UnixMs) -> Self {
        Self {
            config: config.normalized(),
            buckets: vec![Bucket::default(); CAPACITY].into_boxed_slice(),
            points: BTreeMap::new(),
            coverage: BTreeMap::new(),
            connected: false,
            accept_from: now.as_u64() / 1_000,
            last_publish: None,
            dirty: None,
        }
    }

    pub fn points(&self) -> &BTreeMap<UnixMs, PacePoint> {
        &self.points
    }
    pub fn config(&self) -> Config {
        self.config
    }

    /// A missing venue invalidates the composite, rather than appearing as exhaustion.
    /// Resume at the next whole second; partial startup/reconnect seconds are excluded.
    pub fn set_connected(&mut self, connected: bool, now: UnixMs) {
        if connected == self.connected {
            return;
        }
        let second = now.as_u64() / 1_000;
        self.accept_from = second;
        self.connected = connected;
        if connected {
            self.coverage.insert(second + 1, None);
        } else if let Some(end) = self.coverage.last_entry().map(|entry| entry.into_mut()) {
            *end = Some(second);
        }
    }

    pub fn configure(&mut self, config: Config) {
        let config = config.normalized();
        if config != self.config {
            self.config = config;
            self.points.clear();
            // Recompute retained buckets once on a user setting change, never per trade batch.
            self.last_publish = self
                .last_publish
                .map(|end| end.saturating_sub(RETENTION_SECS));
        }
    }

    /// Constant-time bucket updates, independent of tape rate and chart history length.
    pub fn insert(&mut self, trades: &[Trade], qty_is_quote: bool, now: UnixMs) {
        let now = now.as_u64() / 1_000;
        let earliest = self.accept_from.max(now.saturating_sub(RETENTION_SECS));
        for trade in trades {
            let second = trade.time.as_u64() / 1_000;
            let qty = trade.qty.to_f64();
            let price = trade.price.to_f64();
            if second < earliest
                || second > now + 2
                || qty <= 0.0
                || price <= 0.0
                || !qty.is_finite()
                || !price.is_finite()
            {
                continue;
            }
            let bucket = &mut self.buckets[second as usize % CAPACITY];
            if bucket.second != second {
                *bucket = Bucket {
                    second,
                    ..Bucket::default()
                };
            }
            if trade.is_sell {
                bucket.sell += 1;
            } else {
                bucket.buy += 1;
            }
            bucket.notional += if qty_is_quote { qty } else { qty * price };
            self.dirty = Some(self.dirty.map_or((second, second), |(lo, hi)| {
                (lo.min(second), hi.max(second))
            }));
        }
    }

    fn totals(&self, from: u64, to: u64) -> Bucket {
        let mut totals = Bucket::default();
        for second in from..to {
            let bucket = &self.buckets[second as usize % CAPACITY];
            if bucket.second == second {
                totals.buy += bucket.buy;
                totals.sell += bucket.sell;
                totals.notional += bucket.notional;
            }
        }
        totals
    }

    fn point(&self, end: u64) -> PacePoint {
        let window = u64::from(self.config.window_secs);
        let baseline = u64::from(self.config.baseline_secs);
        let from = end.saturating_sub(window);
        let totals = self.totals(from, end);
        let count = totals.buy + totals.sell;
        let segment = self.coverage.range(..=from).next_back();
        let complete = segment.is_some_and(|(_, until)| until.is_none_or(|until| end <= until));
        // Compare with the preceding window, excluding the fast window itself.
        let baseline_from = from.saturating_sub(baseline);
        let baseline = segment
            .filter(|(start, _)| complete && baseline_from >= **start)
            .map(|_| {
                let totals = self.totals(baseline_from, from);
                (totals.buy + totals.sell) as f32 / baseline as f32
            });
        PacePoint {
            time: UnixMs::new(end * 1_000),
            buy: totals.buy as f32 / window as f32,
            sell: totals.sell as f32 / window as f32,
            count,
            average_notional: if count == 0 {
                0.0
            } else {
                totals.notional / count as f64
            },
            baseline,
            complete,
        }
    }

    /// Publish at most once per wall-clock second, including real zero-activity seconds.
    /// Late prints correct only the affected trailing windows, not all retained history.
    pub fn publish(&mut self, now: UnixMs) -> bool {
        let end = now.as_u64() / 1_000;
        if self.last_publish.is_some_and(|last| end <= last) {
            return false;
        }
        let earliest = end.saturating_sub(RETENTION_SECS - 1);
        if let Some((lo, hi)) = self.dirty.take() {
            let last = self.last_publish.unwrap_or(end.saturating_sub(1));
            let tail = hi.saturating_add(u64::from(
                self.config.window_secs + self.config.baseline_secs,
            ));
            for sample in (lo + 1).max(earliest)..=last.min(tail) {
                if self.points.contains_key(&UnixMs::new(sample * 1_000)) {
                    self.points
                        .insert(UnixMs::new(sample * 1_000), self.point(sample));
                }
            }
        }
        let start = self.last_publish.map_or(end, |last| last + 1).max(earliest);
        for sample in start..=end {
            self.points
                .insert(UnixMs::new(sample * 1_000), self.point(sample));
        }
        while self
            .points
            .first_key_value()
            .is_some_and(|(time, _)| time.as_u64() / 1_000 < earliest)
        {
            self.points.pop_first();
        }
        // Retain the interval containing the oldest usable bucket, plus newer gaps.
        let coverage_cutoff = earliest.saturating_sub(660);
        while self
            .coverage
            .first_key_value()
            .is_some_and(|(_, until)| until.is_some_and(|until| until < coverage_cutoff))
        {
            self.coverage.pop_first();
        }
        self.last_publish = Some(end);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::unit::{Price, Qty};

    fn trade(second: u64, sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(second * 1_000 + 100),
            is_sell: sell,
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(2.0),
        }
    }
    fn ready() -> TradePace {
        let mut pace = TradePace::new(Config::default(), UnixMs::new(0));
        pace.set_connected(true, UnixMs::new(0));
        pace
    }
    #[test]
    fn exact_window_baseline_notional_and_multi_venue_sums() {
        let mut pace = ready();
        for second in 1..136 {
            // Same epoch from three venues; none are price/time deduplicated.
            for sell in [false, false, true] {
                pace.insert(
                    &[trade(second, sell)],
                    false,
                    UnixMs::new(second * 1_000 + 500),
                );
            }
            pace.publish(UnixMs::new((second + 1) * 1_000));
        }
        let point = *pace.points.last_key_value().unwrap().1;
        assert!(point.complete);
        assert_eq!(
            (point.buy, point.sell, point.count, point.baseline),
            (2.0, 1.0, 45, Some(3.0))
        );
        assert_eq!(point.average_notional, 200.0);
        assert_eq!(point.relative(), Some(1.0));
    }
    #[test]
    fn quiet_seconds_decay_and_zero_baseline_is_not_infinity() {
        let mut pace = ready();
        pace.insert(&[trade(1, false)], true, UnixMs::new(1_500));
        pace.publish(UnixMs::new(16_000));
        assert_eq!(
            pace.points.last_key_value().unwrap().1.average_notional,
            2.0
        );
        pace.publish(UnixMs::new(140_000));
        let point = *pace.points.last_key_value().unwrap().1;
        assert_eq!(point.total(), 0.0);
        assert_eq!(point.baseline, Some(0.0));
        assert_eq!(point.relative(), None);
    }
    #[test]
    fn startup_and_disconnect_are_gaps_not_zero_activity() {
        let mut pace = ready();
        pace.publish(UnixMs::new(15_000));
        assert!(!pace.points.last_key_value().unwrap().1.complete);
        pace.publish(UnixMs::new(16_000));
        assert!(pace.points.last_key_value().unwrap().1.complete);
        pace.set_connected(false, UnixMs::new(17_000));
        pace.publish(UnixMs::new(18_000));
        assert!(!pace.points.last_key_value().unwrap().1.complete);
        pace.set_connected(true, UnixMs::new(18_000));
        pace.publish(UnixMs::new(33_000));
        assert!(!pace.points.last_key_value().unwrap().1.complete);
        pace.publish(UnixMs::new(34_000));
        assert!(pace.points.last_key_value().unwrap().1.complete);
        assert_eq!(pace.points.last_key_value().unwrap().1.baseline, None);
    }
    #[test]
    fn late_prints_correct_existing_samples_without_replaying_tape() {
        let mut pace = ready();
        pace.publish(UnixMs::new(16_000));
        pace.publish(UnixMs::new(17_000));
        pace.insert(&[trade(15, false)], false, UnixMs::new(17_500));
        assert!(!pace.publish(UnixMs::new(17_900)));
        pace.publish(UnixMs::new(18_000));
        assert_eq!(pace.points[&UnixMs::new(16_000)].count, 1);
        assert_eq!(pace.points[&UnixMs::new(17_000)].count, 1);
    }
    #[test]
    fn reconfiguration_preserves_history_and_does_not_bridge_a_disconnect() {
        let mut pace = ready();
        pace.publish(UnixMs::new(50_000));
        pace.set_connected(false, UnixMs::new(51_000));
        pace.publish(UnixMs::new(52_000));
        pace.set_connected(true, UnixMs::new(53_000));
        pace.publish(UnixMs::new(90_000));
        pace.configure(Config {
            window_secs: 5,
            baseline_secs: 30,
        });
        pace.publish(UnixMs::new(91_000));
        assert!(pace.points[&UnixMs::new(50_000)].complete);
        assert_eq!(pace.points[&UnixMs::new(50_000)].baseline, Some(0.0));
        assert!(!pace.points[&UnixMs::new(52_000)].complete);
        assert!(!pace.points[&UnixMs::new(58_000)].complete);
        assert!(pace.points[&UnixMs::new(59_000)].complete);
        assert_eq!(pace.points[&UnixMs::new(59_000)].baseline, None);
        assert_eq!(pace.points[&UnixMs::new(89_000)].baseline, Some(0.0));
    }
    #[test]
    fn retention_and_config_are_bounded() {
        let mut pace = ready();
        pace.publish(UnixMs::new(1_000));
        pace.publish(UnixMs::new(RETENTION_SECS * 2 * 1_000));
        assert_eq!(pace.points.len(), RETENTION_SECS as usize);
        assert_eq!(pace.buckets.len(), CAPACITY);
        assert_eq!(
            Config {
                window_secs: 0,
                baseline_secs: u16::MAX
            }
            .normalized(),
            Config {
                window_secs: 1,
                baseline_secs: 600
            }
        );
    }

    #[test]
    fn rolling_rates_match_a_tape_oracle_with_late_prints_and_quiet_seconds() {
        let mut pace = ready();
        let mut tape = Vec::new();
        for second in 1..=500 {
            // Deliberately bursty tape, unequal buy/sell counts and genuine quiet seconds.
            let count = if second % 11 == 0 { 0 } else { second % 17 };
            let mut batch: Vec<_> = (0..count)
                .map(|index| trade(second, index % 4 == 0))
                .collect();
            if second > 5 && second % 7 == 0 {
                batch.extend([trade(second - 5, true); 3]);
            }
            tape.extend_from_slice(&batch);
            pace.insert(&batch, false, UnixMs::new(second * 1_000 + 800));
            if second % 4 == 0 {
                pace.publish(UnixMs::new((second + 1) * 1_000));
            }
        }
        for (window_secs, baseline_secs) in [(1, 30), (5, 120), (15, 30), (60, 120)] {
            let end = pace.last_publish.unwrap() + 1;
            pace.configure(Config {
                window_secs,
                baseline_secs,
            });
            pace.publish(UnixMs::new(end * 1_000));
            for second in 200..=500 {
                let point = pace.points()[&UnixMs::new(second * 1_000)];
                let from = second - u64::from(window_secs);
                let fast: Vec<_> = tape
                    .iter()
                    .filter(|trade| {
                        let timestamp = trade.time.as_u64() / 1_000;
                        timestamp >= from && timestamp < second
                    })
                    .collect();
                let sells = fast.iter().filter(|trade| trade.is_sell).count();
                let baseline = tape
                    .iter()
                    .filter(|trade| {
                        let timestamp = trade.time.as_u64() / 1_000;
                        timestamp >= from - u64::from(baseline_secs) && timestamp < from
                    })
                    .count();
                assert!(point.complete);
                assert_eq!(point.count, fast.len() as u64);
                assert_eq!(point.buy, (fast.len() - sells) as f32 / window_secs as f32);
                assert_eq!(point.sell, sells as f32 / window_secs as f32);
                assert_eq!(point.baseline, Some(baseline as f32 / baseline_secs as f32));
                assert_eq!(
                    point.average_notional,
                    if fast.is_empty() { 0.0 } else { 200.0 }
                );
            }
        }
    }

    #[test]
    fn reconnect_rejects_replayed_prints_and_implausible_future_timestamps() {
        let mut pace = ready();
        pace.set_connected(false, UnixMs::new(40_000));
        pace.set_connected(true, UnixMs::new(50_000));
        pace.insert(
            &[
                trade(49, false), // Reconnect snapshot must not re-count old executions.
                trade(50, false), // Partial reconnect second is collected but never marked complete.
                trade(51, true),
                trade(54, false), // More than two seconds ahead of receipt.
                Trade {
                    qty: Qty::ZERO,
                    ..trade(51, false)
                },
                Trade {
                    price: Price::from_f64(-1.0),
                    ..trade(51, false)
                },
            ],
            false,
            UnixMs::new(51_500),
        );
        pace.publish(UnixMs::new(66_000));
        let point = pace.points().last_key_value().unwrap().1;
        assert!(point.complete);
        assert_eq!(point.count, 1);
        assert_eq!(point.buy, 0.0);
        assert_eq!(point.sell, 1.0 / 15.0);
        assert_eq!(point.average_notional, 200.0);
        assert_eq!(point.baseline, None);
    }

    #[test]
    fn persisted_partial_config_has_compatible_defaults() {
        assert_eq!(
            serde_json::from_str::<Config>("{}").unwrap(),
            Config::default()
        );
        assert_eq!(
            serde_json::from_str::<Config>(r#"{"window_secs":30}"#).unwrap(),
            Config {
                window_secs: 30,
                baseline_secs: 120,
            }
        );
    }
    #[test]
    #[ignore = "explicit throughput measurement"]
    fn trade_pace_throughput() {
        let mut pace = ready();
        let batch: Vec<_> = (0..100_000)
            .map(|i| trade(150 + i / 10_000, i % 3 == 0))
            .collect();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            pace.insert(&batch, false, UnixMs::new(160_000));
        }
        let ingest = start.elapsed();
        let start = std::time::Instant::now();
        pace.publish(UnixMs::new(160_000));
        pace.publish(UnixMs::new((RETENTION_SECS + 160) * 1_000));
        println!(
            "trade_pace: 1,000,000 prints ingested in {ingest:?}; 4h publication {:?}; fixed buckets {} bytes; retained samples {}",
            start.elapsed(),
            pace.buckets.len() * std::mem::size_of::<Bucket>(),
            pace.points.len()
        );
        assert!(pace.points.len() <= RETENTION_SECS as usize);
    }
}
