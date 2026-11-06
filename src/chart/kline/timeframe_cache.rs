//! Bounded, move-only cache. A single live journal avoids per-timeframe fan-out.
use crate::connector::fetcher::RequestHandler;
use data::aggr::time::TimeSeries;
use data::chart::kline::{GroupedTrades, KlineDataPoint};
use exchange::{TickerInfo, Timeframe, Trade, UnixMs};
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 5;
const MAX_BARS: usize = 2_048;
const MAX_LIVE_TRADES: usize = 32_768;
// Conservative allocation estimate including hash-table slack and tree nodes.
const MAX_BYTES: usize = 24 * 1024 * 1024;
const ENTRY_OVERHEAD: usize = 128 * 1024; // bounded request map and container overhead

pub(super) struct CachedTimeframe {
    pub series: TimeSeries<KlineDataPoint>,
    pub refresh_from: UnixMs,
    pub requests: RequestHandler,
    pub live_starts: FxHashMap<TickerInfo, UnixMs>,
    cursor: u64,
    saved_at: UnixMs,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct TimeframeCache {
    entries: VecDeque<CachedTimeframe>,
    live: VecDeque<(TickerInfo, Trade)>,
    next_trade: u64,
    skip_active_snapshot: bool,
}

fn bar_bytes(bar: &KlineDataPoint) -> usize {
    std::mem::size_of::<KlineDataPoint>()
        + 128
        + bar.footprint.trades.capacity() * (std::mem::size_of::<GroupedTrades>() + 32) * 2
}

impl TimeframeCache {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.live = VecDeque::new();
        self.next_trade = 0;
        self.skip_active_snapshot = false;
    }

    pub fn break_live_continuity(&mut self) {
        self.live.clear();
        self.next_trade += 1; // older snapshot cursors now precede the live journal
        self.skip_active_snapshot = true;
    }

    pub fn record_live(&mut self, source: TickerInfo, trades: &[Trade]) {
        if self.entries.is_empty() {
            return;
        }
        for trade in trades {
            if self.live.len() == MAX_LIVE_TRADES {
                self.live.pop_front();
            }
            self.live.push_back((source, *trade));
            self.next_trade += 1;
        }
    }

    pub fn take(&mut self, interval: Timeframe) -> Option<CachedTimeframe> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.series.interval == interval)?;
        let mut cached = self.entries.remove(index)?;
        let first_sequence = self.next_trade.saturating_sub(self.live.len() as u64);
        if cached.cursor >= first_sequence {
            let skip = (cached.cursor - first_sequence) as usize;
            let replay = self
                .live
                .iter()
                .skip(skip)
                .map(|(source, trade)| {
                    cached.live_starts.entry(*source).or_insert(trade.time);
                    *trade
                })
                .collect::<Vec<_>>();
            // Source filtering happened before journal insertion. Each journal
            // entry is applied exactly once to this snapshot, including equal
            // executions from separate venues.
            cached.series.insert_trades_or_create_bucket(&replay);
        } else {
            // Away longer than the bounded live journal: reuse closed bars,
            // but refetch the stale tail instead of hiding a gap in one bucket.
            let cutoff = cached.saved_at.floor_to(cached.series.interval);
            cached.series.datapoints.retain(|time, _| *time < cutoff);
            cached.requests = cached.requests.completed_main_trades(|_, to| to < cutoff);
            cached.live_starts.clear();
        }
        (!cached.series.datapoints.is_empty()).then_some(cached)
    }

    pub fn insert(
        &mut self,
        mut series: TimeSeries<KlineDataPoint>,
        now: UnixMs,
        incomplete: &[(UnixMs, UnixMs)],
        requests: &RequestHandler,
        live_starts: &FxHashMap<TickerInfo, UnixMs>,
    ) {
        self.entries
            .retain(|entry| entry.series.interval != series.interval);
        if std::mem::take(&mut self.skip_active_snapshot) {
            return;
        }
        let cutoff = now.floor_to(series.interval);
        // Interrupted pages and live-only prefixes are not evidence of a
        // complete footprint. The live journal keeps other buckets usable.
        series.datapoints.retain(|time, _| {
            !incomplete.iter().any(|(from, to)| {
                *time >= from.floor_to(series.interval) && *time <= to.floor_to(series.interval)
            })
        });
        while series.datapoints.len() > MAX_BARS {
            series.datapoints.pop_first();
        }
        let mut bytes: usize =
            ENTRY_OVERHEAD + series.datapoints.values().map(bar_bytes).sum::<usize>();
        while bytes > MAX_BYTES {
            let Some((_, bar)) = series.datapoints.pop_first() else {
                break;
            };
            bytes = bytes.saturating_sub(bar_bytes(&bar));
        }
        let Some(last) = series.latest_timestamp() else {
            return;
        };
        let refresh_from = cutoff.min(last.saturating_add(series.interval.to_milliseconds()));
        let retained_end = last.saturating_add(series.interval.to_milliseconds());
        let first = *series
            .datapoints
            .first_key_value()
            .expect("nonempty series")
            .0;
        let mut retained_requests = requests.completed_main_trades(|from, to| {
            from >= first
                && to < retained_end
                && !incomplete.iter().any(|(a, b)| {
                    from.floor_to(series.interval) <= b.floor_to(series.interval)
                        && to.floor_to(series.interval) >= a.floor_to(series.interval)
                })
        });
        // Keep the live seam proof even when a broader completed request was
        // excluded by trimming. Otherwise a return can re-fetch that prefix
        // and add it a second time to the retained footprint.
        for (source, time) in live_starts {
            let from = time.floor_to(series.interval);
            let to = time.saturating_sub(1);
            if from < to && series.datapoints.contains_key(&from) {
                retained_requests.copy_completed_trade_prefix(requests, *source, from, to);
            }
        }
        while self.entries.len() >= MAX_ENTRIES
            || self.entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_BYTES
        {
            self.entries.pop_front();
        }
        let live_starts = live_starts
            .iter()
            .filter(|(_, time)| {
                !incomplete.iter().any(|(from, to)| {
                    time.floor_to(series.interval) >= from.floor_to(series.interval)
                        && time.floor_to(series.interval) <= to.floor_to(series.interval)
                })
            })
            .map(|(source, time)| (*source, *time))
            .collect();
        self.entries.push_back(CachedTimeframe {
            series,
            refresh_from,
            requests: retained_requests,
            live_starts,
            cursor: self.next_trade,
            saved_at: now,
            bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::unit::{Price, PriceStep, Qty};
    use exchange::{Kline, Trade, Volume};

    fn series(interval: Timeframe, count: usize) -> TimeSeries<KlineDataPoint> {
        let price = Price::from_f64(100.0);
        let bars = (0..count)
            .map(|i| Kline {
                time: UnixMs::new(i as u64 * interval.to_milliseconds()),
                open: price,
                high: price,
                low: price,
                close: price,
                volume: Volume::empty_buy_sell(),
            })
            .collect::<Vec<_>>();
        let mut series =
            TimeSeries::<KlineDataPoint>::new(interval, PriceStep { units: price.units }, &bars);
        series.insert_trades_existing_buckets(&[Trade {
            time: UnixMs::new(1),
            price,
            qty: Qty::from_f64(2.0),
            is_sell: false,
        }]);
        series
    }

    #[test]
    fn cache_keeps_footprints_and_excludes_partial_buckets() {
        let mut cache = TimeframeCache::default();
        cache.insert(
            series(Timeframe::M1, 4),
            UnixMs::new(210_000),
            &[(UnixMs::new(70_000), UnixMs::new(80_000))],
            &RequestHandler::default(),
            &FxHashMap::default(),
        );
        let restored = cache.take(Timeframe::M1).unwrap();
        assert_eq!(restored.series.datapoints.len(), 3);
        assert!(
            !restored.series.datapoints[&UnixMs::ZERO]
                .footprint
                .trades
                .is_empty()
        );
        assert!(
            restored
                .series
                .datapoints
                .contains_key(&UnixMs::new(120_000))
        );
        assert_eq!(restored.refresh_from, UnixMs::new(180_000));
        assert!(cache.take(Timeframe::M1).is_none());
    }

    fn source() -> TickerInfo {
        TickerInfo::new(
            exchange::Ticker::new("BTCUSDT", exchange::adapter::Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        )
    }

    #[test]
    fn live_journal_replays_each_execution_once_without_refetching_open_footprints() {
        let mut cache = TimeframeCache::default();
        let source = source();
        let requests = RequestHandler::default();
        let starts = FxHashMap::default();
        cache.insert(
            series(Timeframe::M1, 4),
            UnixMs::new(210_000),
            &[],
            &requests,
            &starts,
        );
        let trade = Trade {
            time: UnixMs::new(220_000),
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(3.0),
            is_sell: false,
        };
        cache.record_live(source, &[trade, trade]); // equal prints are distinct executions
        let restored = cache.take(Timeframe::M1).unwrap();
        assert_eq!(
            restored.series.datapoints[&UnixMs::new(180_000)]
                .footprint
                .trades
                .values()
                .map(|g| g.buy_qty.to_f64())
                .sum::<f64>(),
            6.0
        );
        cache.insert(
            restored.series,
            UnixMs::new(230_000),
            &[],
            &restored.requests,
            &restored.live_starts,
        );
        cache.record_live(source, &[trade]);
        let restored = cache.take(Timeframe::M1).unwrap();
        assert_eq!(
            restored.series.datapoints[&UnixMs::new(180_000)]
                .footprint
                .trades
                .values()
                .map(|g| g.buy_qty.to_f64())
                .sum::<f64>(),
            9.0
        );
    }

    #[test]
    fn overflowing_live_journal_invalidates_only_the_stale_tail() {
        let mut cache = TimeframeCache::default();
        cache.insert(
            series(Timeframe::M1, 4),
            UnixMs::new(210_000),
            &[],
            &RequestHandler::default(),
            &FxHashMap::default(),
        );
        let trade = Trade {
            time: UnixMs::new(220_000),
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(1.0),
            is_sell: false,
        };
        cache.record_live(source(), &vec![trade; MAX_LIVE_TRADES + 1]);
        assert_eq!(cache.live.len(), MAX_LIVE_TRADES);
        let restored = cache.take(Timeframe::M1).unwrap();
        assert_eq!(restored.series.datapoints.len(), 3);
        assert!(
            !restored
                .series
                .datapoints
                .contains_key(&UnixMs::new(180_000))
        );
        assert_eq!(restored.refresh_from, UnixMs::new(180_000));
    }

    #[test]
    fn trimming_keeps_live_prefix_proof_and_does_not_suppress_missing_buckets() {
        use crate::connector::fetcher::FetchRange;
        use exchange::adapter::StreamKind;
        let source = source();
        let stream = Some(StreamKind::Trades {
            ticker_info: source,
        });
        let mut requests = RequestHandler::default();
        let id = requests
            .add_request(
                FetchRange::FootprintTrades(UnixMs::ZERO, UnixMs::new(195_000)),
                stream,
            )
            .unwrap()
            .unwrap();
        requests.mark_completed(id);
        let starts = FxHashMap::from_iter([(source, UnixMs::new(190_000))]);
        let mut cache = TimeframeCache::default();
        cache.insert(
            series(Timeframe::M1, 4),
            UnixMs::new(210_000),
            &[(UnixMs::new(70_000), UnixMs::new(80_000))],
            &requests,
            &starts,
        );
        let mut restored = cache.take(Timeframe::M1).unwrap();
        assert!(
            restored
                .requests
                .add_request(
                    FetchRange::FootprintTrades(UnixMs::new(180_000), UnixMs::new(189_999)),
                    stream
                )
                .unwrap()
                .is_none()
        );
        assert!(
            restored
                .requests
                .add_request(
                    FetchRange::FootprintTrades(UnixMs::new(60_000), UnixMs::new(119_999)),
                    stream
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn disconnected_live_stream_cannot_reuse_an_unobserved_tail() {
        let mut cache = TimeframeCache::default();
        let now = UnixMs::new(210_000);
        let requests = RequestHandler::default();
        let starts = FxHashMap::default();
        cache.insert(series(Timeframe::M1, 4), now, &[], &requests, &starts);
        cache.break_live_continuity();
        cache.insert(series(Timeframe::M5, 4), now, &[], &requests, &starts);
        assert!(cache.take(Timeframe::M5).is_none());
        let restored = cache.take(Timeframe::M1).unwrap();
        assert!(
            !restored
                .series
                .datapoints
                .contains_key(&UnixMs::new(180_000))
        );
    }

    fn dense_series(tf: Timeframe, bars: usize, rows: usize) -> TimeSeries<KlineDataPoint> {
        let mut result = series(tf, bars);
        for bar in result.datapoints.values_mut() {
            for row in 0..rows {
                bar.footprint.trades.insert(
                    Price::from_f64(100.0 + row as f64),
                    GroupedTrades::default(),
                );
            }
        }
        result
    }

    #[test]
    fn dense_footprints_respect_total_memory_budget() {
        let mut cache = TimeframeCache::default();
        let now = UnixMs::new(1_000_000_000_000);
        for tf in [Timeframe::M1, Timeframe::M5] {
            cache.insert(
                dense_series(tf, 1_000, 128),
                now,
                &[],
                &RequestHandler::default(),
                &FxHashMap::default(),
            );
            assert!(cache.entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES);
        }
        assert!(cache.take(Timeframe::M1).is_none());
        assert!(cache.take(Timeframe::M5).is_some());
    }

    #[test]
    #[ignore = "manual cache latency measurement"]
    fn timeframe_cache_switch_latency() {
        let mut cache = TimeframeCache::default();
        let now = UnixMs::new(1_000_000_000_000);
        let frames = [
            Timeframe::M1,
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::H1,
            Timeframe::H4,
        ];
        for tf in frames {
            cache.insert(
                dense_series(tf, 1_000, 16),
                now,
                &[],
                &RequestHandler::default(),
                &FxHashMap::default(),
            );
        }
        let trades = vec![
            Trade {
                time: now,
                price: Price::from_f64(100.0),
                qty: Qty::from_f64(1.0),
                is_sell: false
            };
            100
        ];
        let mut samples = Vec::new();
        for index in 0..200 {
            let start = std::time::Instant::now();
            cache.record_live(source(), &trades);
            let cached = cache
                .take(frames[index % frames.len()])
                .expect("bounded warm cache");
            cache.insert(
                cached.series,
                now,
                &[],
                &cached.requests,
                &cached.live_starts,
            );
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        eprintln!(
            "timeframe cache: 1000 bars x 16 rows, 100 new live trades per switch; median={}us p95={}us max={}us; cache={} bytes, journal_capacity={} bytes",
            samples[100],
            samples[190],
            samples[199],
            cache.entries.iter().map(|entry| entry.bytes).sum::<usize>(),
            cache.live.capacity() * std::mem::size_of::<(TickerInfo, Trade)>()
        );
    }

    #[test]
    fn cache_bounds_bars_and_evicts_least_recently_used_timeframe() {
        let mut cache = TimeframeCache::default();
        let now = UnixMs::new(10_000_000_000);
        for tf in [
            Timeframe::M1,
            Timeframe::M5,
            Timeframe::M15,
            Timeframe::H1,
            Timeframe::H4,
            Timeframe::D1,
        ] {
            cache.insert(
                series(tf, 10),
                now,
                &[],
                &RequestHandler::default(),
                &FxHashMap::default(),
            );
        }
        assert!(cache.take(Timeframe::M1).is_none());
        cache.insert(
            series(Timeframe::M1, MAX_BARS + 50),
            now,
            &[],
            &RequestHandler::default(),
            &FxHashMap::default(),
        );
        assert_eq!(
            cache.take(Timeframe::M1).unwrap().series.datapoints.len(),
            MAX_BARS
        );
        assert!(cache.entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_BYTES);
    }
}
