use super::*;
use data::aggr::ticks::TrevSnapshot;

// Recover only the recent window, in larger serial batches with a brief yield.
const TREV_REPLAY_PAGE_MS: u64 = 15 * 60 * 1_000;
const TREV_REPLAY_PAUSE: Duration = Duration::from_millis(100);

/// Opt-in, bounded CPU profiling of the real native canvas. The normal app
/// takes no timestamps or locks unless FLOWSURFACE_TREV_PERF=1 at startup.
pub(super) struct DrawMeasurement(Instant);

impl Drop for DrawMeasurement {
    fn drop(&mut self) {
        struct Samples {
            values: Vec<u64>,
            since: Instant,
        }
        static SAMPLES: std::sync::OnceLock<std::sync::Mutex<Samples>> = std::sync::OnceLock::new();
        let elapsed = self.0.elapsed().as_nanos() as u64;
        let samples = SAMPLES.get_or_init(|| {
            std::sync::Mutex::new(Samples {
                values: Vec::with_capacity(512),
                since: Instant::now(),
            })
        });
        if let Ok(mut samples) = samples.lock() {
            if samples.values.len() < 512 {
                samples.values.push(elapsed);
            }
            if samples.since.elapsed() >= Duration::from_secs(5) && !samples.values.is_empty() {
                samples.values.sort_unstable();
                let n = samples.values.len();
                log::info!(
                    "TRev draw CPU: samples={} p50_us={:.1} p95_us={:.1} max_us={:.1}",
                    n,
                    samples.values[n / 2] as f64 / 1000.,
                    samples.values[n * 95 / 100] as f64 / 1000.,
                    samples.values[n - 1] as f64 / 1000.
                );
                samples.values.clear();
                samples.since = Instant::now();
            }
        }
    }
}

pub(super) fn draw_measurement(kind: &KlineChartKind) -> Option<DrawMeasurement> {
    if !matches!(kind, KlineChartKind::Trev { .. }) {
        return None;
    }
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ENABLED
        .get_or_init(|| std::env::var("FLOWSURFACE_TREV_PERF").as_deref() == Ok("1"))
        .then(|| DrawMeasurement(Instant::now()))
}

pub(super) struct TrevRuntime {
    pub pending: bool,
    request: Option<uuid::Uuid>,
    replay: Option<TickAggr>,
    replay_needs_segment: bool,
    replay_needed_segment_at_start: bool,
    cursor: Option<UnixMs>,
    next_request_at: Option<Instant>,
    live: Vec<Trade>,
    live_started: Option<Instant>,
    history_tail: Vec<Trade>,
    from: Option<UnixMs>,
    through: Option<UnixMs>,
    boundary: FxHashMap<(i64, i64, bool), usize>,
}

impl Default for TrevRuntime {
    fn default() -> Self {
        Self {
            pending: true,
            request: None,
            replay: None,
            replay_needs_segment: false,
            replay_needed_segment_at_start: false,
            cursor: None,
            next_request_at: None,
            live: Vec::new(),
            live_started: None,
            history_tail: Vec::new(),
            from: None,
            through: None,
            boundary: FxHashMap::default(),
        }
    }
}

impl KlineChart {
    pub fn trev_catchup_label(&self) -> Option<String> {
        if !matches!(self.kind, KlineChartKind::Trev { .. }) || !self.trev.pending {
            return None;
        }
        let time = match &self.data_source {
            PlotData::TickBased(aggr) => aggr.trev_last_input_time(),
            PlotData::TimeBased(_) => None,
        }
        .and_then(|time| chrono::DateTime::from_timestamp_millis(time.as_u64() as i64));
        Some(time.map_or_else(
            || "Loading TRev history...".to_owned(),
            |time| format!("Loading history: {} UTC", time.format("%m-%d %H:%M")),
        ))
    }

    pub fn trev_snapshot(&self) -> Option<Box<TrevSnapshot>> {
        match &self.data_source {
            PlotData::TickBased(aggr) => aggr.trev_snapshot(self.chart.ticker_info.ticker),
            PlotData::TimeBased(_) => None,
        }
    }

    pub fn restore_trev_snapshot(&mut self, snapshot: &TrevSnapshot) -> bool {
        self.restore_trev_snapshot_at(snapshot, UnixMs::now())
    }

    fn restore_trev_snapshot_at(&mut self, snapshot: &TrevSnapshot, now: UnixMs) -> bool {
        let KlineChartKind::Trev { config } = self.kind else {
            return false;
        };
        let Some(mut aggr) = TickAggr::restore_trev(
            snapshot,
            self.chart.ticker_info.ticker,
            config,
            PriceStep::from(self.chart.ticker_info.min_ticksize),
        ) else {
            return false;
        };
        if aggr
            .trev_last_input_time()
            .is_some_and(|last| last < now.saturating_sub(TickAggr::TREV_HISTORY_WINDOW_MS))
        {
            aggr = TickAggr::new_trev(
                config,
                PriceStep::from(self.chart.ticker_info.min_ticksize),
                &[],
            );
        }
        aggr.trim_trev_history(now);
        log::info!(
            "Restored TRev {}: {} confirmed bars through {:?}",
            self.chart.ticker_info.ticker,
            aggr.datapoints.len().saturating_sub(1),
            aggr.trev_last_input_time()
        );
        self.data_source = PlotData::TickBased(aggr);
        *self.trev = Default::default();
        self.trade_history_loaded = false;
        self.publish_trev();
        true
    }

    fn publish_trev(&mut self) {
        if let PlotData::TickBased(aggr) = &self.data_source {
            self.chart.last_price = aggr
                .datapoints
                .last()
                .map(|dp| PriceInfoLabel::new(dp.kline.close, dp.kline.open));
        }
        self.indicators
            .values_mut()
            .filter_map(Option::as_mut)
            .for_each(|indicator| indicator.on_basis_change(&self.data_source));
        self.invalidate(None);
    }

    pub(super) fn buffer_trev_live(&mut self, buffer: &[Trade]) -> bool {
        if !matches!(self.kind, KlineChartKind::Trev { .. }) || !self.trev.pending {
            return false;
        }
        if !buffer.is_empty() {
            self.trev.live_started.get_or_insert_with(Instant::now);
        }
        self.trev.live.extend_from_slice(buffer);
        const MAX_BUFFER: usize = 80_000;
        if self.trev.live.len() > MAX_BUFFER {
            let excess = self.trev.live.len() - MAX_BUFFER;
            self.trev.live.drain(..excess);
        }
        if let Some(trade) = buffer.last() {
            self.chart.last_price = Some(PriceInfoLabel::new(trade.price, trade.price));
        }
        self.invalidate_live_render();
        true
    }

    pub(super) fn fetch_trev_history(&mut self) -> Option<Action> {
        self.plan_trev_history(trade_fetch_mode())
    }

    fn plan_trev_history(&mut self, mode: TradeFetchMode) -> Option<Action> {
        if !self.trev.pending
            || self.trev.request.is_some()
            || self
                .trev
                .next_request_at
                .is_some_and(|ready| Instant::now() < ready)
        {
            return None;
        }
        // Include the first live millisecond in the fetch. Its complete exchange
        // history replaces that boundary millisecond of the buffered live tail.
        let live_boundary = self.trev.live.iter().map(|t| t.time).min()?;
        let newest_live = self.trev.live.iter().map(|t| t.time).max()?;
        let cutoff = newest_live.saturating_sub(TickAggr::TREV_HISTORY_WINDOW_MS);
        if let PlotData::TickBased(aggr) = &mut self.data_source {
            if aggr
                .trev_last_input_time()
                .is_some_and(|last| last < cutoff)
            {
                let KlineChartKind::Trev { config } = self.kind else {
                    return None;
                };
                *aggr = TickAggr::new_trev(
                    config,
                    PriceStep::from(self.chart.ticker_info.min_ticksize),
                    &[],
                );
                self.trev.replay_needs_segment = false;
            }
            let trimmed = aggr.trim_trev_history(newest_live);
            if aggr.trev_last_input_time().is_none() {
                self.trev.replay_needs_segment = false;
            }
            if trimmed {
                self.publish_trev();
            }
        }
        if self.trev.cursor.is_some_and(|cursor| cursor < cutoff) {
            self.trev.cursor = None;
        }
        let source = self.feed.primary();
        let recorder = matches!(mode, TradeFetchMode::Server);
        if !trade_history_available(source, mode) {
            self.finish_trev_history(None, false);
            return None;
        }
        // The recorder durably commits its newest executions after receipt.
        // Wait from local arrival, since exchange timestamps can lead our clock.
        // Otherwise a normal trailing commit delay looks like an actual outage.
        if recorder
            && self
                .trev
                .live_started
                .is_some_and(|started| started.elapsed() < Duration::from_secs(5))
        {
            return None;
        }
        let PlotData::TickBased(aggr) = &self.data_source else {
            return None;
        };
        let last_time = aggr.trev_last_input_time();
        let from_time = self.trev.cursor.or(last_time).unwrap_or(cutoff).max(cutoff);
        let through = from_time
            .saturating_add(TREV_REPLAY_PAGE_MS)
            .min(live_boundary.max(from_time));
        // Reuse the complete executed-history request: a capped generic seed
        // cannot certify a restart gap or silently connect recorder holes.
        let fetch = FetchRange::TrevTrades(from_time, through);
        let stream = StreamKind::Trades {
            ticker_info: source,
        };
        let Ok(Some(req_id)) = self.request_handler.add_request(fetch, Some(stream)) else {
            return None;
        };
        self.trev.from = Some(from_time);
        self.trev.through = Some(through);
        self.trev.boundary.clear();
        self.trev.history_tail.clear();
        if !self.trev.replay_needs_segment && last_time == Some(from_time) {
            for t in aggr.trev_boundary_trades() {
                *self
                    .trev
                    .boundary
                    .entry((
                        t.price.round_to_min_tick(source.min_ticksize).units,
                        t.qty.units,
                        t.is_sell,
                    ))
                    .or_default() += 1;
            }
        }
        self.trev.replay = Some(aggr.clone());
        self.trev.replay_needed_segment_at_start = self.trev.replay_needs_segment;
        self.trev.request = Some(req_id);
        let specs = vec![FetchSpec {
            req_id,
            fetch,
            stream: Some(stream),
        }];
        self.begin_trade_fetches(&specs);
        Some(Action::RequestFetch(specs))
    }

    pub(super) fn cancel_trev_request(&mut self) {
        if self.trev.replay.is_some() {
            self.trev.replay_needs_segment = self.trev.replay_needed_segment_at_start;
        }
        self.trev.request = None;
        self.trev.replay = None;
        self.trev.boundary.clear();
        self.trev.history_tail.clear();
    }

    pub(super) fn insert_trev_history(&mut self, req_id: Option<uuid::Uuid>, trades: &[Trade]) {
        if req_id.is_none() || req_id != self.trev.request {
            return;
        }
        let min_tick = self.chart.ticker_info.min_ticksize;
        let replay = trades
            .iter()
            .copied()
            .map(|mut trade| {
                // Live adapters normalize prices to the exchange tick. Arrow
                // f64 decoding can otherwise differ by one atomic price unit,
                // falsely failing the saved-millisecond overlap check.
                trade.price = trade.price.round_to_min_tick(min_tick);
                trade
            })
            .filter(|t| {
                if self.trev.from.is_some_and(|from| t.time < from)
                    || self.trev.through.is_some_and(|through| t.time > through)
                {
                    return false;
                }
                if Some(t.time) == self.trev.from
                    && let Some(count) =
                        self.trev
                            .boundary
                            .get_mut(&(t.price.units, t.qty.units, t.is_sell))
                    && *count > 0
                {
                    *count -= 1;
                    return false;
                }
                true
            })
            .collect::<Vec<_>>();
        if let Some(aggr) = &mut self.trev.replay {
            if self.trev.replay_needs_segment && !replay.is_empty() {
                aggr.resume_trev_after_gap(&replay);
                self.trev.replay_needs_segment = false;
            } else {
                aggr.insert_trades(&replay);
            }
        }
        self.trev.history_tail.extend_from_slice(&replay);
        let excess = self.trev.history_tail.len().saturating_sub(80_000);
        if excess > 0 {
            self.trev.history_tail.drain(..excess);
        }
    }

    pub(super) fn finish_partial_trev_history(
        &mut self,
        req_id: uuid::Uuid,
        missing_ranges: &[(UnixMs, UnixMs)],
    ) -> bool {
        if self.trev.request != Some(req_id) {
            return false;
        }
        if let Some(last_gap) = missing_ranges.iter().map(|(_, to)| *to).max() {
            // Only the contiguous proven suffix after the last gap can become
            // a new segment. All earlier confirmed bars remain untouched.
            self.trev.history_tail.retain(|t| t.time > last_gap);
        }
        self.finish_trev_history(Some(req_id), false)
    }

    pub(super) fn fail_trev_history(&mut self, req_id: uuid::Uuid) -> bool {
        if self.trev.request != Some(req_id) {
            return false;
        }
        self.cancel_trev_request();
        self.trev.next_request_at = Some(Instant::now() + Duration::from_secs(31));
        self.finish_trade_fetch(req_id);
        // The saved prefix remains visible; a network failure must not become
        // a permanently skipped interval or an immediate retry loop.
        true
    }

    pub(super) fn finish_trev_history(
        &mut self,
        req_id: Option<uuid::Uuid>,
        complete: bool,
    ) -> bool {
        if !matches!(self.kind, KlineChartKind::Trev { .. })
            || !self.trev.pending
            || req_id != self.trev.request
        {
            return false;
        }
        let complete = complete && self.trev.boundary.values().all(|count| *count == 0);
        if complete && let Some(aggr) = self.trev.replay.take() {
            self.data_source = PlotData::TickBased(aggr);
        } else if let PlotData::TickBased(aggr) = &mut self.data_source {
            self.trev.replay = None;
            // Do not connect an unproven outage with synthetic price movement.
            // Preserve every confirmed bar and start a separate live segment.
            log::warn!(
                "TRev {}: incomplete trade coverage; preserving confirmed bars and keeping the unavailable interval as a gap",
                self.chart.ticker_info.ticker
            );
            let boundary = aggr.trev_last_input_time();
            self.trev
                .history_tail
                .retain(|t| boundary.is_none_or(|from| t.time > from));
            aggr.resume_trev_after_gap(&self.trev.history_tail);
            self.trev.replay_needs_segment = self.trev.history_tail.is_empty();
        }

        let through = self.trev.through;
        let caught_up = through.is_none_or(|end| {
            self.trev
                .live
                .iter()
                .map(|t| t.time)
                .min()
                .is_none_or(|first| first <= end)
        });
        if caught_up {
            let mut live = std::mem::take(&mut self.trev.live);
            live.sort_by_key(|t| t.time);
            if let PlotData::TickBased(aggr) = &mut self.data_source {
                let boundary = aggr.trev_last_input_time();
                if self.trev.replay_needs_segment {
                    live.retain(|t| boundary.is_none_or(|from| t.time > from));
                    aggr.resume_trev_after_gap(&live);
                } else {
                    live.retain(|t| through.is_none_or(|end| t.time > end));
                    aggr.insert_trades(&live);
                }
            }
            *self.trev = TrevRuntime {
                pending: false,
                ..Default::default()
            };
            self.trade_history_loaded = true;
        } else {
            // Publish/save each complete page. If the bounded live buffer has
            // advanced during a long catch-up, fetch the intervening trades too.
            self.trev.cursor = through.map(|end| end.saturating_add(1));
            self.trev.next_request_at = Some(Instant::now() + TREV_REPLAY_PAUSE);
            self.cancel_trev_request();
        }
        if let Some(req_id) = req_id {
            self.finish_trade_fetch(req_id);
        }
        self.publish_trev();
        if let PlotData::TickBased(aggr) = &self.data_source {
            log::info!(
                "TRev {}: {} confirmed bars through {:?}; catch-up {}",
                self.chart.ticker_info.ticker,
                aggr.datapoints.len().saturating_sub(1),
                aggr.trev_last_input_time(),
                if caught_up { "complete" } else { "continuing" }
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, adapter::Exchange, unit::Qty};

    fn source() -> TickerInfo {
        TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        )
    }
    fn config() -> data::chart::trev::Config {
        data::chart::trev::Config {
            continuation_ticks: 200,
            reversal_ticks: 300,
            normalization_ms: 250,
            ..Default::default()
        }
    }
    fn trade(t: u64, p: f64) -> Trade {
        Trade {
            time: UnixMs::new(t),
            price: Price::from_f64(p),
            qty: Qty::from_f64(1.),
            is_sell: false,
        }
    }
    fn chart() -> KlineChart {
        KlineChart::new(
            ViewConfig::default(),
            Basis::Tick(data::aggr::TickCount(1)),
            PriceStep::from(source().min_ticksize),
            &[],
            vec![],
            &[],
            source(),
            &KlineChartKind::Trev { config: config() },
            None,
        )
    }
    fn snapshot(c: &KlineChart) -> String {
        serde_json::to_string(&c.trev_snapshot()).unwrap()
    }
    fn restore(c: &mut KlineChart, saved: &TrevSnapshot) -> bool {
        c.restore_trev_snapshot_at(saved, UnixMs::ZERO)
    }
    fn expected(trades: &[Trade]) -> String {
        serde_json::to_string(
            &TickAggr::new_trev(config(), PriceStep::from(source().min_ticksize), trades)
                .trev_snapshot(source().ticker),
        )
        .unwrap()
    }
    fn request(c: &mut KlineChart) -> FetchSpec {
        let Some(Action::RequestFetch(mut specs)) = c.plan_trev_history(TradeFetchMode::Exchange)
        else {
            panic!("Trev must fetch executions before other history");
        };
        assert_eq!(specs.len(), 1);
        specs.remove(0)
    }

    #[test]
    fn trev_history_pages_and_live_overlap_match_uninterrupted_trades() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(500, 140.),
            trade(750, 160.),
            trade(1000, 100.),
            trade(1250, 80.),
        ];
        let mut c = chart();
        c.insert_trades(source(), &trades[4..]);
        let spec = request(&mut c);
        assert!(matches!(spec.fetch, FetchRange::TrevTrades(..)));
        c.insert_raw_trades(source(), trades[..2].to_vec(), false, Some(spec.req_id));
        assert!(
            c.trev_snapshot().is_none(),
            "partial pages stay unpublished"
        );
        c.insert_raw_trades(source(), trades[2..5].to_vec(), false, Some(spec.req_id));
        c.finalize_trade_fetch(spec.req_id);
        assert_eq!(snapshot(&c), expected(&trades));
        assert!(!c.trev.pending);
        assert!(!c.is_fetching_trades());
        c.insert_raw_trades(source(), trades.to_vec(), true, Some(spec.req_id));
        assert_eq!(
            snapshot(&c),
            expected(&trades),
            "late pages cannot replay confirmed trades"
        );
    }

    #[test]
    fn trev_restart_preserves_equal_millisecond_print_multiplicity() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(250, 120.),
            trade(250, 120.),
            trade(500, 160.),
            trade(500, 160.),
            trade(750, 140.),
        ];
        let before = TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &trades[..3],
        );
        let saved = before.trev_snapshot(source().ticker).unwrap();
        let mut c = chart();
        assert!(restore(&mut c, &saved));
        let original = snapshot(&c);
        c.insert_trades(source(), &trades[4..]);
        let spec = request(&mut c);
        assert!(
            matches!(spec.fetch, FetchRange::TrevTrades(from, to) if from == UnixMs::new(250) && to == UnixMs::new(500))
        );
        for t in &trades[1..6] {
            c.insert_raw_trades(source(), vec![*t], false, Some(spec.req_id));
            assert_eq!(snapshot(&c), original);
        }
        c.finalize_trade_fetch(spec.req_id);
        assert_eq!(snapshot(&c), expected(&trades));
        let roundtrip: TrevSnapshot =
            serde_json::from_str(&serde_json::to_string(&c.trev_snapshot().unwrap()).unwrap())
                .unwrap();
        let mut again = chart();
        assert!(restore(&mut again, &roundtrip));
        assert_eq!(snapshot(&again), snapshot(&c));
    }

    #[test]
    fn trev_restart_matches_arrow_prices_to_the_live_exchange_tick() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(500, 140.),
            trade(750, 160.),
        ];
        let before = TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &trades[..2],
        );
        let mut c = chart();
        assert!(restore(
            &mut c,
            &before.trev_snapshot(source().ticker).unwrap()
        ));
        c.insert_trades(source(), &trades[2..]);
        let spec = request(&mut c);
        let historical = trades[1..3]
            .iter()
            .map(|t| Trade {
                price: Price::from_units(t.price.units + 1),
                ..*t
            })
            .collect();
        c.insert_raw_trades(source(), historical, false, Some(spec.req_id));
        c.finalize_trade_fetch(spec.req_id);
        assert_eq!(snapshot(&c), expected(&trades));
    }

    #[test]
    fn trev_partial_history_and_late_ohlc_never_rewrite_confirmed_bricks() {
        let mut c = chart();
        let before = TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &[trade(0, 100.), trade(250, 120.)],
        );
        assert!(restore(
            &mut c,
            &before.trev_snapshot(source().ticker).unwrap()
        ));
        c.insert_trades(source(), &[trade(100_000, 1000.), trade(100_250, 1020.)]);
        let spec = request(&mut c);
        c.insert_raw_trades(
            source(),
            vec![trade(50_000, 500.)],
            false,
            Some(spec.req_id),
        );
        c.finalize_partial_trade_fetch(
            spec.req_id,
            source(),
            &[(UnixMs::new(1000), UnixMs::new(100_000))],
        );
        let PlotData::TickBased(aggr) = &c.data_source else {
            panic!()
        };
        assert_eq!(aggr.datapoints.len(), 3);
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f64(120.));
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f64(1000.));
        let saved = snapshot(&c);
        c.insert_hist_klines(
            uuid::Uuid::new_v4(),
            source(),
            &[Kline {
                time: UnixMs::ZERO,
                open: Price::from_f64(1.),
                high: Price::from_f64(2000.),
                low: Price::from_f64(1.),
                close: Price::from_f64(2000.),
                volume: exchange::Volume::empty_buy_sell(),
            }],
        );
        assert_eq!(snapshot(&c), saved);
        assert!(!c.accepts_tick_kline_seed(Timeframe::M1));
    }

    #[test]
    fn trev_canceled_history_can_be_requested_again() {
        let mut c = chart();
        c.insert_trades(source(), &[trade(1_000, 100.)]);
        let first = request(&mut c);
        c.reset_request_handler();
        let second = request(&mut c);
        assert_ne!(first.req_id, second.req_id);
        c.mark_fetch_failed(second.req_id);
        assert!(
            c.trev.pending,
            "failures remain retryable, without skipping the outage"
        );
        assert!(c.trev.request.is_none());
        assert!(
            c.plan_trev_history(TradeFetchMode::Exchange).is_none(),
            "retry must back off"
        );
        assert!(!c.is_fetching_trades());
    }

    #[test]
    fn trev_waits_for_recorder_commit_before_fetching_the_live_boundary() {
        let mut c = chart();
        c.insert_trades(source(), &[trade(1_000, 100.)]);
        assert!(c.plan_trev_history(TradeFetchMode::Server).is_none());
        assert!(c.trev.pending);
        assert!(c.trev.request.is_none());
        c.trev.live_started = Some(Instant::now() - Duration::from_secs(5));
        assert!(matches!(
            c.plan_trev_history(TradeFetchMode::Server),
            Some(Action::RequestFetch(_))
        ));
    }

    #[test]
    fn trev_long_outage_fills_every_page_and_resumes_a_saved_partial_catchup() {
        let mut c = chart();
        let trades: Vec<_> = (0..=50)
            .map(|i| trade(i * 60_000, 100. + (i % 11) as f64 * 20.))
            .collect();
        let before = TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &trades[..2],
        );
        assert!(restore(
            &mut c,
            &before.trev_snapshot(source().ticker).unwrap()
        ));
        c.insert_trades(source(), &trades[50..]);
        let mut pages = 0;
        while c.trev.pending {
            let spec = request(&mut c);
            let FetchRange::TrevTrades(from, to) = spec.fetch else {
                panic!()
            };
            assert!(to.saturating_diff(from) <= TREV_REPLAY_PAGE_MS);
            if pages == 0 {
                assert_eq!(from, trades[1].time);
            }
            let rows = trades
                .iter()
                .copied()
                .filter(|t| t.time >= from && t.time <= to)
                .collect();
            c.insert_raw_trades(source(), rows, false, Some(spec.req_id));
            c.finalize_trade_fetch(spec.req_id);
            pages += 1;
            if c.trev.pending {
                assert!(
                    c.plan_trev_history(TradeFetchMode::Exchange).is_none(),
                    "pages are paced"
                );
                c.trev.next_request_at = None;
            }
            if pages == 2 {
                let saved: TrevSnapshot = serde_json::from_str(
                    &serde_json::to_string(&c.trev_snapshot().unwrap()).unwrap(),
                )
                .unwrap();
                c = chart();
                assert!(restore(&mut c, &saved));
                c.insert_trades(source(), &trades[50..]);
            }
            assert!(pages < 20);
        }
        assert!(pages >= 4);
        assert_eq!(snapshot(&c), expected(&trades));
        let mut fresh = chart();
        fresh.insert_trades(source(), &trades[50..]);
        let initial = request(&mut fresh);
        assert!(matches!(initial.fetch, FetchRange::TrevTrades(from, to)
            if to.saturating_diff(from) == TREV_REPLAY_PAGE_MS));
    }

    #[test]
    fn trev_live_buffer_rollover_extends_catchup_without_losing_executions() {
        let mut all = vec![trade(0, 100.), trade(250, 120.), trade(1000, 140.)];
        all.extend((1001..=81_002).map(|time| trade(time, 140.)));
        let before =
            TickAggr::new_trev(config(), PriceStep::from(source().min_ticksize), &all[..2]);
        let mut c = chart();
        assert!(restore(
            &mut c,
            &before.trev_snapshot(source().ticker).unwrap()
        ));
        c.insert_trades(source(), &all[2..3]);
        let first = request(&mut c);
        c.insert_trades(source(), &all[3..]);
        assert_eq!(c.trev.live.len(), 80_000);
        c.insert_raw_trades(source(), all[1..3].to_vec(), false, Some(first.req_id));
        c.finalize_trade_fetch(first.req_id);
        assert!(
            c.trev.pending,
            "the old handoff boundary was evicted from the live buffer"
        );
        c.trev.next_request_at = None;
        let second = request(&mut c);
        let FetchRange::TrevTrades(from, to) = second.fetch else {
            panic!()
        };
        assert_eq!(from, UnixMs::new(1001));
        c.insert_raw_trades(
            source(),
            all.iter()
                .copied()
                .filter(|t| t.time >= from && t.time <= to)
                .collect(),
            false,
            Some(second.req_id),
        );
        c.finalize_trade_fetch(second.req_id);
        assert!(!c.trev.pending);
        assert_eq!(snapshot(&c), expected(&all));
    }

    #[test]
    fn trev_stale_and_empty_charts_load_only_the_latest_four_hours() {
        let now = UnixMs::new(24 * 60 * 60 * 1_000);
        let cutoff = now.saturating_sub(TickAggr::TREV_HISTORY_WINDOW_MS);
        let old = TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &[trade(0, 100.), trade(250, 120.)],
        );
        let saved = old.trev_snapshot(source().ticker).unwrap();
        let recent = (0..=16)
            .map(|i| {
                trade(
                    cutoff.as_u64() + i * TREV_REPLAY_PAGE_MS,
                    1_000. + (i % 5) as f64 * 20.,
                )
            })
            .collect::<Vec<_>>();

        // Neither an expired saved chain nor an empty chart should fetch the
        // overnight outage or connect today's prices to yesterday's anchor.
        for old_checkpoint in [true, false] {
            let mut c = chart();
            if old_checkpoint {
                assert!(restore(&mut c, &saved));
            }
            c.insert_trades(source(), &recent[16..]);
            let mut pages = 0;
            while c.trev.pending {
                let spec = request(&mut c);
                let FetchRange::TrevTrades(from, to) = spec.fetch else {
                    panic!()
                };
                assert!(from >= cutoff && to <= now);
                if pages == 0 {
                    assert_eq!(from, cutoff);
                    assert!(c.trev_snapshot().is_none(), "expired bricks were removed");
                }
                let batch = recent
                    .iter()
                    .copied()
                    .filter(|t| t.time >= from && t.time <= to)
                    .collect();
                c.insert_raw_trades(source(), batch, false, Some(spec.req_id));
                c.finalize_trade_fetch(spec.req_id);
                c.trev.next_request_at = None;
                pages += 1;
                assert!(pages <= 16);
            }
            assert_eq!(pages, 16);
            assert_eq!(snapshot(&c), expected(&recent));
        }

        let mut restored = chart();
        assert!(restored.restore_trev_snapshot_at(&saved, now));
        assert!(
            restored.trev_snapshot().is_none(),
            "expired cache is not shown on startup"
        );
    }

    #[test]
    fn trev_reconnect_replays_only_missing_executions() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(500, 140.),
            trade(750, 160.),
            trade(1000, 120.),
        ];
        let mut c = chart();
        c.data_source = PlotData::TickBased(TickAggr::new_trev(
            config(),
            PriceStep::from(source().min_ticksize),
            &trades[..2],
        ));
        c.trev.pending = false;
        let saved = snapshot(&c);
        c.break_cached_trade_continuity();
        assert_eq!(snapshot(&c), saved);
        c.insert_trades(source(), &trades[4..]);
        let spec = request(&mut c);
        c.insert_raw_trades(source(), trades[1..].to_vec(), false, Some(spec.req_id));
        c.finalize_trade_fetch(spec.req_id);
        assert_eq!(snapshot(&c), expected(&trades));
        c.set_trev_config(config());
        assert_eq!(
            snapshot(&c),
            expected(&trades),
            "same settings must not erase history"
        );
    }

    #[test]
    fn trev_partial_seed_uses_only_the_contiguous_proven_suffix() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(500, 140.),
            trade(750, 160.),
            trade(1000, 160.),
        ];
        let mut c = chart();
        c.insert_trades(source(), &trades[4..]);
        let spec = request(&mut c);
        c.insert_raw_trades(source(), trades.to_vec(), false, Some(spec.req_id));
        c.finalize_partial_trade_fetch(
            spec.req_id,
            source(),
            &[(UnixMs::new(100), UnixMs::new(450))],
        );
        assert_eq!(snapshot(&c), expected(&trades[2..]));
    }

    #[test]
    fn trev_display_changes_preserve_bars_and_foreign_config_events_are_ignored() {
        let mut c = chart();
        c.trev.pending = false;
        c.insert_trades(
            source(),
            &[trade(0, 100.), trade(250, 120.), trade(500, 140.)],
        );
        let before = snapshot(&c);
        assert!(
            c.raw_trades().is_empty(),
            "TRev must not duplicate its exact books into a rolling raw tape"
        );
        c.set_trev_config(data::chart::trev::Config {
            display: data::chart::trev::Display::BidAsk,
            ticks_per_row: 50,
            ..config()
        });
        assert_eq!(snapshot(&c), before);
        assert_eq!(
            c.chart.decimals,
            PriceStep::from(source().min_ticksize).decimal_places()
        );
        assert!(!c.trev.pending);
        c.set_renko_config(RenkoConfig::default());
        c.set_tpo_config(TpoConfig::default());
        assert_eq!(snapshot(&c), before);
        assert!(matches!(c.kind(), KlineChartKind::Trev { .. }));
        assert_eq!(
            c.tick_size().units,
            PriceStep::from(source().min_ticksize).units * 50
        );
    }

    #[test]
    fn trev_reconfiguration_cannot_mutate_another_chart_or_accept_old_requests() {
        let trades = [trade(0, 100.), trade(250, 120.), trade(500, 140.)];
        let mut other = chart();
        other.trev.pending = false;
        other.insert_trades(source(), &trades);
        let other_before = snapshot(&other);
        let mut renko = KlineChart::new(
            ViewConfig::default(),
            Basis::Tick(data::aggr::TickCount(1)),
            PriceStep::from(source().min_ticksize),
            &[],
            vec![],
            &[],
            source(),
            &KlineChartKind::Renko {
                config: RenkoConfig::default(),
            },
            None,
        );
        renko.renko.pending = false;
        renko.insert_trades(source(), &trades);
        let renko_before = serde_json::to_string(&renko.renko_snapshot()).unwrap();
        let mut changing = chart();
        changing.insert_trades(source(), &trades[2..]);
        let request = request(&mut changing);
        changing.set_trev_config(data::chart::trev::Config {
            continuation_ticks: 420,
            reversal_ticks: 260,
            ..config()
        });
        let reset = snapshot(&changing);
        changing.insert_raw_trades(source(), trades.to_vec(), true, Some(request.req_id));
        changing.finalize_trade_fetch(request.req_id);
        assert_eq!(snapshot(&changing), reset);
        renko.set_trev_config(config());
        assert_eq!(
            serde_json::to_string(&renko.renko_snapshot()).unwrap(),
            renko_before
        );
        assert_eq!(snapshot(&other), other_before);
    }

    #[test]
    fn restored_trev_footprints_keep_the_latest_column_inside_the_viewport() {
        let trades = [
            trade(0, 100.),
            trade(250, 120.),
            trade(500, 140.),
            trade(750, 120.),
        ];
        for display in [
            data::chart::trev::Display::BidAsk,
            data::chart::trev::Display::Volume,
            data::chart::trev::Display::Delta,
        ] {
            let config = data::chart::trev::Config {
                display,
                ticks_per_row: 50,
                ..config()
            };
            let aggr = TickAggr::new_trev(config, PriceStep::from(source().min_ticksize), &trades);
            let mut restored = KlineChart::new(
                ViewConfig::default(),
                Basis::Tick(data::aggr::TickCount(1)),
                PriceStep::from(source().min_ticksize),
                &[],
                vec![],
                &[],
                source(),
                &KlineChartKind::Trev { config },
                None,
            );
            assert!(restore(
                &mut restored,
                &aggr.trev_snapshot(source().ticker).unwrap()
            ));
            crate::chart::update(
                &mut restored,
                &crate::chart::Message::BoundsChanged(Rectangle {
                    x: 0.,
                    y: 0.,
                    width: 1200.,
                    height: 650.,
                }),
            );
            assert!(
                restored.chart.is_interval_x_visible(0.),
                "newest {display} column must be visible immediately after restore"
            );
            let visible = restored.chart.visible_region(restored.chart.bounds.size());
            let (first, last) = restored.chart.interval_range(&visible);
            assert_eq!(first, 0);
            assert!(
                restored
                    .data_source
                    .visible_price_range(first, last)
                    .is_some()
            );
        }
    }
}
