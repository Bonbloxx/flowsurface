//! Independent catalog sources, a shared compact book and bounded sequential
//! replay. The Binance-only path in the parent remains unchanged.
use super::*;
use data::aggregation::{AggregateFeedId, ResolvedFeed};

const WINDOW_MS: u64 = 3_600_000;
const STALE_MS: u64 = 10_000;

pub(super) struct Replay {
    pub id: Option<uuid::Uuid>,
    pub handle: Option<iced::task::Handle>,
    from: UnixMs,
    end: UnixMs,
    window_from: UnixMs,
    window_to: UnixMs,
    source_index: usize,
    tape: Tape,
    source_tape: Tape,
    detector: Detector,
    missing: Vec<(UnixMs, UnixMs)>,
    gap_count: usize,
    max_book_bytes: usize,
    max_replay_us: u128,
}

impl Replay {
    fn new(from: UnixMs, end: UnixMs, config: orderflow::Config, step: PriceStep) -> Self {
        Self {
            id: None,
            handle: None,
            from,
            end,
            window_from: from,
            window_to: from.saturating_add(WINDOW_MS - 1).min(end),
            source_index: 0,
            tape: Tape::default(),
            source_tape: Tape::default(),
            detector: Detector::new(config, step),
            missing: Vec::new(),
            gap_count: 0,
            max_book_bytes: 0,
            max_replay_us: 0,
        }
    }
    pub fn owns(&self, id: uuid::Uuid) -> bool {
        self.id == Some(id)
    }
    pub fn begin(&mut self, id: uuid::Uuid) {
        self.id = Some(id);
    }
    pub fn stage(
        &mut self,
        id: uuid::Uuid,
        source: TickerInfo,
        sources: &[TickerInfo],
        trades: &[Trade],
        quote: bool,
    ) {
        if !self.owns(id) || sources.get(self.source_index) != Some(&source) {
            return;
        }
        // Close a source's seconds before aligning its price premium. Paging
        // and out-of-order archive fallback cannot change normalization.
        self.source_tape.insert(trades, self.detector.step(), quote);
        self.max_book_bytes = self
            .max_book_bytes
            .max(self.tape.estimated_bytes() + self.source_tape.estimated_bytes());
    }
    fn finish(&mut self, sources: usize, missing: &[(UnixMs, UnixMs)]) -> bool {
        let started = std::time::Instant::now();
        self.id = None;
        self.handle = None;
        self.tape.prints += self.source_tape.prints;
        if self.source_index == 0 {
            self.tape.seconds.append(&mut self.source_tape.seconds);
        } else {
            for (time, second) in &self.source_tape.seconds {
                if let Some(reference) = self.tape.seconds.get_mut(time) {
                    reference.add_venue_flow(second);
                }
            }
        }
        self.source_tape = Tape::default();
        self.missing.extend_from_slice(missing);
        self.gap_count += missing.len();
        self.source_index += 1;
        if self.source_index < sources {
            self.max_replay_us = self.max_replay_us.max(started.elapsed().as_micros());
            return false;
        }
        self.missing.sort_unstable();
        let latest = self.tape.seconds.last_key_value().map_or(0, |(&t, _)| t);
        for second in self
            .tape
            .seconds
            .values()
            .filter(|second| second.time < latest.saturating_sub(1_000))
        {
            let after = self.detector.last_time().map_or(0, |t| t + 1_000);
            if second.time < after {
                continue;
            }
            if self
                .missing
                .iter()
                .any(|(from, to)| from.as_u64() <= second.time + 999 && to.as_u64() >= after)
            {
                self.detector.reset_continuity();
            }
            if !self
                .missing
                .iter()
                .any(|(from, to)| from.as_u64() <= second.time + 999 && to.as_u64() >= second.time)
            {
                self.detector.process(second);
            }
        }
        // A gap at the end of a window must also cancel pending evidence even
        // when there is no execution after it to carry the reset.
        if self
            .missing
            .iter()
            .any(|(_, to)| to.as_u64() >= self.detector.last_time().unwrap_or(0))
        {
            self.detector.reset_continuity();
        }
        self.tape.seconds.retain(|time, _| {
            *time >= latest.saturating_sub(1_000)
                && !self
                    .missing
                    .iter()
                    .any(|(from, to)| from.as_u64() <= *time + 999 && to.as_u64() >= *time)
        });
        self.missing.clear();
        self.max_replay_us = self.max_replay_us.max(started.elapsed().as_micros());
        if self.window_to == self.end {
            return true;
        }
        self.window_from = self.window_to.saturating_add(1);
        self.window_to = self.window_from.saturating_add(WINDOW_MS - 1).min(self.end);
        self.source_index = 0;
        false
    }
}

impl OrderflowIndicator {
    pub fn binance_only(&self) -> bool {
        self.sources.len() == 1 && self.sources[0].exchange() == Exchange::BinanceLinear
    }
    pub fn accepts(&self, source: TickerInfo) -> bool {
        self.sources.contains(&source)
    }
    pub fn configure_sources(&mut self, candidates: Vec<TickerInfo>) -> bool {
        let Some(primary) = self.source else {
            return false;
        };
        let Some(id) = AggregateFeedId::for_seed_ticker(primary.ticker) else {
            return false;
        };
        let selected = candidates
            .iter()
            .map(|source| source.ticker)
            .collect::<Vec<_>>();
        let feed = ResolvedFeed::aggregated_selected(id, primary, &candidates, Some(&selected));
        if self.sources == feed.sources() {
            return false;
        }
        self.sources = feed.sources().to_vec();
        self.watermarks = vec![None; self.sources.len()];
        self.source_tapes = (0..self.sources.len()).map(|_| Tape::default()).collect();
        self.history = None;
        self.aggregate_history = None;
        self.covered_from = None;
        self.history_anchor = None;
        self.tape = Tape::default();
        self.notice = None;
        self.rebuild();
        log::info!("orderflow_sources {}", self.source_label());
        true
    }
    pub fn plan_fetch(&mut self, now: UnixMs) -> Option<(TickerInfo, UnixMs, UnixMs)> {
        if self.binance_only() {
            let (from, to) = self.plan_history(now)?;
            return Some((self.sources[0], from, to));
        }
        if self.covered_from.is_some() || !self.source.is_some_and(Self::supports) {
            return None;
        }
        if self.aggregate_history.is_none() {
            let (from, to) = self.plan_history(now)?;
            self.aggregate_history = Some(Replay::new(
                from,
                to,
                self.config,
                self.detector.as_ref()?.step(),
            ));
        }
        let replay = self.aggregate_history.as_ref()?;
        if replay.id.is_some() {
            return None;
        }
        self.notice = Some(format!(
            "Loading 4-day orderflow · {} · {}%",
            self.source_label(),
            (replay.window_from.as_u64() - replay.from.as_u64()) * 100
                / (replay.end.as_u64() - replay.from.as_u64()).max(1)
        ));
        Some((
            self.sources[replay.source_index],
            replay.window_from,
            replay.window_to,
        ))
    }
    pub fn finish_partial(&mut self, id: uuid::Uuid, missing: &[(UnixMs, UnixMs)]) -> bool {
        if self
            .aggregate_history
            .as_ref()
            .is_some_and(|replay| replay.owns(id))
        {
            self.finish_aggregate(id, true, missing)
        } else {
            self.finish(id, false)
        }
    }
    pub(super) fn finish_aggregate(
        &mut self,
        id: uuid::Uuid,
        success: bool,
        missing: &[(UnixMs, UnixMs)],
    ) -> bool {
        let Some(replay) = self
            .aggregate_history
            .as_mut()
            .filter(|replay| replay.owns(id))
        else {
            return false;
        };
        if !success {
            // Retry only the current window; never publish an incomplete mix or
            // re-download the already accepted earlier hours.
            replay.id = None;
            replay.handle = None;
            replay.source_tape = Tape::default();
            self.notice = Some("Orderflow history unavailable; retrying current hour".into());
            return true;
        }
        if !replay.finish(self.sources.len(), missing) {
            return true;
        }
        let mut replay = self.aggregate_history.take().unwrap();
        let tail = self
            .tape
            .seconds
            .split_off(&(replay.end.as_u64() / 1_000 * 1_000 + 1_000));
        replay.tape.seconds.extend(tail);
        self.tape = replay.tape;
        self.detector = Some(replay.detector);
        self.covered_from = Some(replay.from.as_u64());
        self.notice = (replay.gap_count > 0).then(|| {
            format!(
                "Orderflow: {} recorder gaps excluded; warm-up restarts after gaps",
                replay.gap_count
            )
        });
        self.advance_aggregate();
        log::info!(
            "orderflow_aggregate_history sources={} from={} to={} prints={} seconds={} marks={} gaps={} max_book_bytes={} max_replay_us={}",
            self.source_label(),
            replay.from,
            replay.end,
            self.tape.prints,
            self.detector.as_ref().unwrap().evaluated_seconds,
            self.detector.as_ref().unwrap().events().len(),
            replay.gap_count,
            replay.max_book_bytes,
            replay.max_replay_us
        );
        true
    }
    pub(super) fn source_label(&self) -> String {
        self.sources
            .iter()
            .map(|source| source.exchange().venue().to_string())
            .collect::<Vec<_>>()
            .join(" + ")
    }
    pub(super) fn insert_aggregate_live(&mut self, source: TickerInfo, trades: &[Trade]) -> bool {
        let Some(index) = self
            .sources
            .iter()
            .position(|candidate| *candidate == source)
        else {
            return false;
        };
        let Some(latest) = trades.iter().map(|trade| trade.time.as_u64()).max() else {
            return false;
        };
        self.watermarks[index] = Some(self.watermarks[index].unwrap_or(0).max(latest));
        let newest = self
            .watermarks
            .iter()
            .flatten()
            .copied()
            .max()
            .unwrap_or(latest);
        if self
            .watermarks
            .iter()
            .flatten()
            .any(|time| newest.saturating_sub(*time) > STALE_MS)
        {
            self.detector.as_mut().unwrap().reset_continuity();
            self.tape = Tape::default();
            self.watermarks.fill(None);
            for tape in &mut self.source_tapes {
                *tape = Tape::default();
            }
            self.watermarks[index] = Some(latest);
            self.notice =
                Some("Orderflow paused: waiting for all selected venues; warm-up restarts".into());
        }
        let quote = self.qty_is_quote();
        let detector = self.detector.as_ref().unwrap();
        self.source_tapes[index].insert_after(trades, detector.step(), quote, detector.last_time());
        let oldest = newest.saturating_sub(STALE_MS);
        for tape in &mut self.source_tapes {
            tape.seconds.retain(|time, _| *time >= oldest);
        }
        self.advance_aggregate()
    }
    fn advance_aggregate(&mut self) -> bool {
        let Some(before) = self
            .watermarks
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .and_then(|times| times.into_iter().min())
            .map(|time| (time / 1_000 * 1_000).saturating_sub(1_000))
        else {
            return false;
        };
        let detector = self.detector.as_mut().unwrap();
        let previous = detector.last_time();
        let from = previous.map_or(0, |time| time + 1);
        if from < before {
            for (&time, reference) in self.source_tapes[0].seconds.range(from..before) {
                let mut second = reference.clone();
                for tape in &self.source_tapes[1..] {
                    if let Some(other) = tape.seconds.get(&time) {
                        second.add_venue_flow(other);
                    }
                }
                detector.process(&second);
                self.tape.seconds.insert(time, second);
            }
        }
        for tape in &mut self.source_tapes {
            tape.seconds.retain(|time, _| *time >= before);
        }
        // No per-venue retained book. Replay/window state and live input are
        // independent, and live summaries have only ten minutes of retention.
        let oldest = before.saturating_sub(2 * orderflow::WARMUP_MS);
        self.tape.seconds.retain(|time, _| *time >= oldest);
        if previous != detector.last_time()
            && self
                .notice
                .as_ref()
                .is_some_and(|notice| notice.starts_with("Orderflow paused"))
        {
            self.notice = None;
        }
        previous != detector.last_time()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        Ticker,
        unit::{Price, Qty},
    };

    fn sources() -> Vec<TickerInfo> {
        AggregateFeedId::BtcUsdtPerpetual
            .definition()
            .sources
            .iter()
            .map(|source| TickerInfo::new(source.ticker(), 0.1, 0.001, None))
            .collect()
    }
    fn trades() -> Vec<Trade> {
        (0..=380)
            .map(|second| {
                let (price, usd, sell) = match second {
                    0..300 => (1_000.0, 100.0, second % 2 == 0),
                    300..360 => (1_000.0 + (second - 300) as f64 * 0.6, 100.0, false),
                    360..364 => (1_040.0, 150_000.0, false),
                    _ => (1_025.0, 100.0, true),
                };
                Trade {
                    time: UnixMs::new(second * 1_000),
                    price: Price::from_f64(price),
                    qty: Qty::from_f64(usd / price),
                    is_sell: sell,
                }
            })
            .collect()
    }
    fn finish_window(
        indicator: &mut OrderflowIndicator,
        source: TickerInfo,
        trades: &[Trade],
        missing: &[(UnixMs, UnixMs)],
    ) {
        let (planned, from, to) = indicator
            .plan_fetch(UnixMs::new(orderflow::HISTORY_MS + orderflow::WARMUP_MS))
            .unwrap();
        assert_eq!(planned, source);
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, from, to);
        indicator.stage(id, source, trades);
        assert!(indicator.finish_partial(id, missing));
    }
    #[test]
    fn catalog_selections_are_independent_and_merge_the_same_notional_as_ordered_tape() {
        let sources = sources();
        let trades = trades();
        for mask in 1..8 {
            let wanted = sources
                .iter()
                .enumerate()
                .filter_map(|(i, source)| (mask & (1 << i) != 0).then_some(*source))
                .collect::<Vec<_>>();
            let mut indicator = OrderflowIndicator::default();
            indicator.configure(orderflow::Config::default(), sources[0]);
            indicator.configure_sources(wanted.clone());
            assert_eq!(indicator.sources, wanted);
            if indicator.binance_only() {
                continue;
            }
            for &source in &wanted {
                finish_window(&mut indicator, source, &trades, &[]);
            }
            let replay = indicator.aggregate_history.as_ref().unwrap();
            let mut tape = Tape::default();
            tape.insert(&trades, replay.detector.step(), false);
            let other = tape.seconds.clone();
            for _ in &wanted[1..] {
                for (time, second) in &other {
                    tape.seconds.get_mut(time).unwrap().add_venue_flow(second);
                }
            }
            let mut expected = Detector::new(orderflow::Config::default(), replay.detector.step());
            advance_detector(&tape, &mut expected);
            assert!(!expected.events().is_empty());
            assert_eq!(
                format!("{:?}", replay.detector.events()),
                format!("{:?}", expected.events()),
                "mask {mask}"
            );
            assert!(replay.tape.seconds.len() <= 2);
        }
    }
    #[test]
    fn gaps_and_missing_or_stale_live_venues_cannot_confirm_exhaustion() {
        let sources = sources();
        let trades = trades();
        let mut indicator = OrderflowIndicator::default();
        indicator.configure(orderflow::Config::default(), sources[0]);
        indicator.configure_sources(sources[..2].to_vec());
        finish_window(&mut indicator, sources[0], &trades, &[]);
        finish_window(
            &mut indicator,
            sources[1],
            &trades,
            &[(UnixMs::new(360_000), UnixMs::new(367_999))],
        );
        assert!(
            indicator
                .aggregate_history
                .as_ref()
                .unwrap()
                .detector
                .events()
                .is_empty()
        );
        assert!(!indicator.insert_live(sources[0], &trades));
        assert!(indicator.detector.as_ref().unwrap().events().is_empty());
        indicator.insert_live(sources[1], &trades[..310]);
        assert!(indicator.detector.as_ref().unwrap().events().is_empty());
        assert!(indicator.notice.as_ref().unwrap().contains("paused"));
        assert!(indicator.tape.seconds.len() <= 601);
    }

    #[test]
    fn live_and_history_agree_and_a_venue_price_premium_cannot_move_reference_extrema() {
        let sources = sources();
        let reference = trades();
        let other = reference
            .iter()
            .map(|trade| Trade {
                price: Price::from_f64(trade.price.to_f64() + 250.0),
                qty: Qty::from_f64(trade.qty.to_f64() * trade.price.to_f64()),
                ..*trade
            })
            .collect::<Vec<_>>();
        // Quote sizes make notional exactly invariant to the added premium.
        let reference = reference
            .iter()
            .map(|trade| Trade {
                qty: Qty::from_f64(trade.qty.to_f64() * trade.price.to_f64()),
                ..*trade
            })
            .collect::<Vec<_>>();
        let step = sources[0].min_ticksize.into();
        let mut reference_tape = Tape::default();
        reference_tape.insert(&reference, step, true);
        let mut other_tape = Tape::default();
        other_tape.insert(&other, step, true);
        let mut expected = Detector::new(orderflow::Config::default(), step);
        for (time, second) in &reference_tape.seconds {
            let mut merged = second.clone();
            merged.add_venue_flow(&other_tape.seconds[time]);
            assert_eq!(
                (merged.first, merged.last, merged.high, merged.low),
                (second.first, second.last, second.high, second.low)
            );
            assert_eq!(merged.volume.total(), second.volume.total() * 2.0);
            assert!(merged.levels.len() <= orderflow::MAX_LEVELS_PER_SECOND);
            assert!(
                merged
                    .levels
                    .iter()
                    .all(|(price, _)| price.to_f64() < 1_100.0)
            );
            if *time < 379_000 {
                expected.process(&merged);
            }
        }
        assert!(!expected.events().is_empty());
        let mut live = OrderflowIndicator::default();
        live.configure(orderflow::Config::default(), sources[0]);
        live.configure_sources(sources[..2].to_vec());
        // Feed base sizes to the normal adapter-unit path, with equal native
        // premiums on BOTH venues; the live/history fixture below uses base.
        let base = trades();
        for trade in &base {
            for &source in &sources[..2] {
                live.insert_live(source, &[*trade]);
            }
        }
        let mut history = OrderflowIndicator::default();
        history.configure(orderflow::Config::default(), sources[0]);
        history.configure_sources(sources[..2].to_vec());
        for &source in &sources[..2] {
            finish_window(&mut history, source, &base, &[]);
        }
        assert_eq!(
            format!("{:?}", live.detector.as_ref().unwrap().events()),
            format!(
                "{:?}",
                history
                    .aggregate_history
                    .as_ref()
                    .unwrap()
                    .detector
                    .events()
            )
        );
    }
    #[test]
    fn failed_source_rolls_back_only_its_pages_without_recounting_prior_sources() {
        let sources = sources();
        let trades = trades();
        let mut indicator = OrderflowIndicator::default();
        indicator.configure(orderflow::Config::default(), sources[0]);
        indicator.configure_sources(sources[..2].to_vec());
        finish_window(&mut indicator, sources[0], &trades, &[]);
        let (source, from, to) = indicator.plan_fetch(UnixMs::new(400_000_000)).unwrap();
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, from, to);
        indicator.stage(id, source, &trades[..100]);
        indicator.finish(id, false);
        assert_eq!(
            indicator.plan_fetch(UnixMs::new(500_000_000)),
            Some((source, from, to))
        );
        assert_eq!(
            indicator.aggregate_history.as_ref().unwrap().tape.prints,
            trades.len() as u64
        );
        finish_window(&mut indicator, sources[1], &trades, &[]);
        assert_eq!(
            indicator.aggregate_history.as_ref().unwrap().tape.prints,
            2 * trades.len() as u64
        );
    }
    #[test]
    #[ignore = "bounded real-data audit; set ORDERFLOW_REPLAY_CSV to a frozen recorder export"]
    fn recorded_binance_replay_survives_round_trip_source_toggles() {
        use std::io::{BufRead, BufReader};
        let file = std::fs::File::open(std::env::var("ORDERFLOW_REPLAY_CSV").unwrap()).unwrap();
        let trades = BufReader::new(file)
            .lines()
            .map(|line| {
                let line = line.unwrap();
                let f = line.split(',').collect::<Vec<_>>();
                Trade {
                    time: UnixMs::new(f[5].parse().unwrap()),
                    price: Price::from_f64(f[1].parse().unwrap()),
                    qty: Qty::from_f64(f[2].parse().unwrap()),
                    is_sell: f[6] == "true",
                }
            })
            .collect::<Vec<_>>();
        assert!(trades.len() <= 2_000_000);
        let sources = sources();
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let mut indicator = OrderflowIndicator::default();
        indicator.configure(orderflow::Config::default(), binance);
        indicator.configure_sources(sources);
        indicator.configure_sources(vec![binance]);
        let mut expected_tape = Tape::default();
        expected_tape.insert(&trades, binance.min_ticksize.into(), false);
        let mut expected = Detector::new(orderflow::Config::default(), binance.min_ticksize.into());
        advance_detector(&expected_tape, &mut expected);
        let id = uuid::Uuid::new_v4();
        indicator.begin_history(id, trades[0].time, trades.last().unwrap().time);
        for batch in trades.chunks(10_000) {
            indicator.stage(id, binance, batch);
        }
        indicator.finish(id, true);
        let actual = indicator.detector.as_ref().unwrap();
        assert!(!actual.events().is_empty());
        assert_eq!(
            format!("{:?}", actual.events()),
            format!("{:?}", expected.events())
        );
        for (a, b) in actual.events().iter().zip(expected.events()) {
            assert_eq!(marker_radius(a), marker_radius(b));
        }
        println!(
            "Exact Binance round-trip match: {} executions, {} full signal records and severity sizes",
            trades.len(),
            actual.events().len()
        );
    }
}
