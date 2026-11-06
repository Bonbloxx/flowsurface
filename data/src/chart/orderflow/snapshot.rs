use super::{BREAK_WINDOW_MS, Detector, Event, MAX_EVENTS, POLICY_VERSION, RETENTION_MS};
use exchange::{Ticker, unit::PriceStep};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Frozen confirmed observations, persisted with the pane. This is not a
/// coverage checkpoint: detection still warms up and fetches execution history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    version: u8,
    policy: String,
    sources: Vec<Ticker>,
    step: PriceStep,
    qty_is_quote: bool,
    events: Vec<Event>,
}

impl Snapshot {
    pub fn new(
        sources: Vec<Ticker>,
        step: PriceStep,
        qty_is_quote: bool,
        detector: &Detector,
        now: u64,
    ) -> Option<Self> {
        let events = detector
            .events()
            .iter()
            .filter(|event| valid_event(event, now))
            .cloned()
            .collect::<Vec<_>>();
        (!events.is_empty()).then(|| Self {
            version: 1,
            policy: POLICY_VERSION.into(),
            sources,
            step,
            qty_is_quote,
            events,
        })
    }

    pub fn matches(&self, sources: &[Ticker], step: PriceStep, qty_is_quote: bool) -> bool {
        self.version == 1
            && self.policy == POLICY_VERSION
            && self.step == step
            && self.qty_is_quote == qty_is_quote
            && self.sources.len() == sources.len()
            && self
                .sources
                .iter()
                .zip(sources)
                .all(|(a, b)| a.same_market(b))
            && self.events.len() <= MAX_EVENTS
    }

    pub fn restore(&self, detector: &mut Detector, now: u64) {
        detector.merge_confirmed_events(&self.events, now);
    }
}

fn valid_event(event: &Event, now: u64) -> bool {
    let Some(confirmed) = event.confirmed else {
        return false;
    };
    event.observed >= now.saturating_sub(RETENTION_MS)
        && confirmed > event.observed
        && confirmed <= now
        && event
            .broken
            .is_none_or(|broken| broken >= confirmed && broken <= now)
        && event.price.units > 0
        && event
            .confirmation_price
            .is_some_and(|price| price.units > 0)
        && [
            event.buy_usd,
            event.sell_usd,
            event.threshold_usd,
            event.pace_ratio,
            event.rejection,
            event.band_width,
            event.activity_ratio,
            event.reaction_activity_usd,
            event.push_usd,
        ]
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0)
        && event.band_width > 0.0
}

impl Detector {
    /// Keep the saved observation's frozen evidence. Replay may establish an
    /// outcome, but missing replay input cannot delete an observed confirmation.
    pub fn merge_confirmed_events(&mut self, saved: &[Event], now: u64) {
        let mut merged: Vec<Event> = Vec::new();
        let mut indices = HashMap::new();
        for event in saved.iter().chain(&self.events) {
            if !valid_event(event, now) {
                continue;
            }
            let key = (event.kind, event.bullish, event.observed, event.price);
            if let Some(&index) = indices.get(&key) {
                let existing: &mut Event = &mut merged[index];
                existing.broken = existing.broken.or(event.broken.filter(|broken| {
                    existing.confirmed.is_some_and(|confirmed| {
                        *broken >= confirmed
                            && *broken <= confirmed.saturating_add(BREAK_WINDOW_MS + 999)
                    })
                }));
            } else {
                indices.insert(key, merged.len());
                merged.push(event.clone());
            }
        }
        merged.sort_by_key(|event| (event.observed, event.confirmed));
        let excess = merged.len().saturating_sub(MAX_EVENTS);
        self.events = merged.into_iter().skip(excess).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::orderflow::{Config, Kind};
    use exchange::{adapter::Exchange, unit::Price};

    fn source() -> Ticker {
        Ticker::new("BTCUSDT", Exchange::BinanceLinear)
    }

    fn step() -> PriceStep {
        PriceStep {
            units: Price::from_f64(0.1).units,
        }
    }

    fn event(observed: u64) -> Event {
        Event {
            kind: Kind::Absorption,
            bullish: true,
            price: Price::from_f64(100_000.0),
            observed,
            confirmed: Some(observed + 2_000),
            broken: None,
            buy_usd: 100_000.0,
            sell_usd: 1_000_000.0,
            threshold_usd: 800_000.0,
            pace_ratio: 0.35,
            rejection: 20.0,
            confirmation_price: Some(Price::from_f64(100_025.0)),
            band_width: 5.0,
            activity_ratio: 2.0,
            reaction_activity_usd: 100_000.0,
            push_usd: 1_000_000.0,
        }
    }

    #[test]
    fn restart_roundtrip_preserves_frozen_confirmations_without_restoring_detection_continuity() {
        let now = 1_000_000;
        let mut before = Detector::new(Config::default(), step());
        let confirmed = event(now - 10_000);
        let mut broken = event(now - 20_000);
        broken.kind = Kind::Exhaustion;
        broken.broken = Some(now - 5_000);
        before.merge_confirmed_events(&[confirmed.clone(), broken.clone()], now);
        let saved = Snapshot::new(vec![source()], step(), false, &before, now).unwrap();
        let restarted: Snapshot =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        let mut after = Detector::new(Config::default(), step());
        restarted.restore(&mut after, now);
        assert_eq!(
            after.events().iter().cloned().collect::<Vec<_>>(),
            [broken, confirmed]
        );
        assert_eq!(after.last_time(), None);
        assert_eq!(after.evaluated_seconds, 0);
        assert!(after.pending().is_empty());
        after.reset_continuity();
        assert_eq!(
            after.events().len(),
            2,
            "recorder gaps must retain saved facts"
        );
    }

    #[test]
    fn replay_merges_outcomes_without_replacing_frozen_evidence_or_duplicating_markers() {
        let now = 1_000_000;
        let saved = event(now - 10_000);
        let mut replayed = saved.clone();
        replayed.threshold_usd = 2_000_000.0;
        replayed.confirmed = Some(now - 7_000);
        replayed.broken = Some(now - 5_000);
        let additional = event(now - 4_000);
        let mut replay = Detector::new(Config::default(), step());
        replay.merge_confirmed_events(&[replayed, additional.clone()], now);
        replay.merge_confirmed_events(&[saved.clone(), saved.clone()], now);
        replay.merge_confirmed_events(std::slice::from_ref(&saved), now);
        assert_eq!(replay.events().len(), 2);
        let restored = &replay.events()[0];
        assert_eq!(restored.confirmed, saved.confirmed);
        assert_eq!(restored.threshold_usd, saved.threshold_usd);
        assert_eq!(restored.broken, Some(now - 5_000));
        assert_eq!(replay.events()[1], additional);
    }

    #[test]
    fn snapshot_rejects_other_sources_policy_units_and_bad_or_expired_events() {
        let now = RETENTION_MS + 1_000_000;
        let mut detector = Detector::new(Config::default(), step());
        let valid = event(now - 10_000);
        let mut pending = valid.clone();
        pending.confirmed = None;
        let mut invalid = valid.clone();
        invalid.band_width = f64::NAN;
        detector.merge_confirmed_events(&[event(1), event(now + 1), pending, invalid, valid], now);
        assert_eq!(detector.events().len(), 1);
        let mut saved = Snapshot::new(vec![source()], step(), false, &detector, now).unwrap();
        assert!(saved.matches(&[source()], step(), false));
        assert!(!saved.matches(
            &[Ticker::new("ETHUSDT", Exchange::BinanceLinear)],
            step(),
            false
        ));
        assert!(!saved.matches(
            &[source()],
            PriceStep {
                units: step().units * 10
            },
            false
        ));
        assert!(!saved.matches(&[source()], step(), true));
        saved.policy = "different-detector".into();
        assert!(!saved.matches(&[source()], step(), false));
        let mut after = Detector::new(Config::default(), step());
        saved.restore(&mut after, now + RETENTION_MS);
        assert!(after.events().is_empty());
    }

    #[test]
    fn replayed_outcome_before_frozen_confirmation_cannot_invalidate_the_saved_marker() {
        let now = 1_000_000;
        let mut saved = event(now - 10_000);
        saved.confirmed = Some(now - 2_000);
        let mut replayed = saved.clone();
        replayed.confirmed = Some(now - 8_000);
        replayed.broken = Some(now - 5_000);
        let mut detector = Detector::new(Config::default(), step());
        detector.merge_confirmed_events(&[replayed], now);
        detector.merge_confirmed_events(std::slice::from_ref(&saved), now);
        assert_eq!(detector.events()[0], saved);
        assert!(Snapshot::new(vec![source()], step(), false, &detector, now).is_some());
    }

    #[test]
    fn restored_event_retention_is_bounded() {
        let now = 10_000_000;
        let events = (0..MAX_EVENTS + 100)
            .map(|index| event(index as u64 * 1_000))
            .collect::<Vec<_>>();
        let mut detector = Detector::new(Config::default(), step());
        detector.merge_confirmed_events(&events, now);
        assert_eq!(detector.events().len(), MAX_EVENTS);
        assert_eq!(detector.events()[0].observed, 100_000);
    }
}
