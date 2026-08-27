use crate::chart::{
    Basis, Caches, Message, ViewState,
    indicator::{
        indicator_row,
        kline::{AvailabilityCause, FetchCtx, IndicatorAvailability, KlineIndicatorImpl},
        plot::{AnySeries, PlotTooltip, candle::CandlePlot},
    },
};
use crate::connector::fetcher::FetchRange;

use data::chart::{PlotData, kline::KlineDataPoint};
use data::util::{abbr_large_numbers, format_with_commas};
use exchange::adapter::{Exchange, Venue};
use exchange::{Kline, TickerInfo, Timeframe, Trade, UnixMs};
use rustc_hash::FxHashMap;

use iced::widget::{center, row, text};
use std::{
    collections::{BTreeMap, btree_map},
    iter::Peekable,
    ops::RangeInclusive,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenInterestCandle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub source_count: usize,
    pub expected_source_count: usize,
}

fn change_text(value: &OpenInterestCandle) -> String {
    let delta = value.close - value.open;
    let sign = if delta >= 0.0 { "+" } else { "-" };
    format!("Change: {sign}${}", format_with_commas(delta.abs()))
}

pub struct OpenInterestIndicator {
    cache: Caches,
    pub data: BTreeMap<UnixMs, OpenInterestCandle>,
    sources: Vec<TickerInfo>,
    source_data: FxHashMap<TickerInfo, BTreeMap<UnixMs, f64>>,
    timeframe: Option<Timeframe>,
    /// Wall-clock time of the last developing-bucket re-poll, so the freshest
    /// OI candle tracks venue revisions instead of freezing between buckets.
    last_refresh_request: Option<UnixMs>,
}

struct SourceCursor<'a> {
    source: TickerInfo,
    current: Option<(UnixMs, f64)>,
    upcoming: Peekable<btree_map::Range<'a, UnixMs, f64>>,
}

impl<'a> SourceCursor<'a> {
    fn new(source: TickerInfo, series: &'a BTreeMap<UnixMs, f64>, from: UnixMs) -> Self {
        Self {
            source,
            current: series
                .range(..from)
                .next_back()
                .map(|(time, value)| (*time, *value)),
            upcoming: series.range(from..).peekable(),
        }
    }

    fn next_time(&mut self) -> Option<UnixMs> {
        self.upcoming.peek().map(|(time, _)| **time)
    }

    fn advance_through(&mut self, time: UnixMs) {
        while self.next_time().is_some_and(|next| next <= time) {
            let (sample_time, value) = self
                .upcoming
                .next()
                .expect("peeked OI observation remains available");
            self.current = Some((*sample_time, *value));
        }
    }
}

impl OpenInterestIndicator {
    /// How often the still-forming bucket is re-polled beyond bucket-boundary
    /// fetches, so the newest OI candle reflects venue revisions.
    const DEVELOPING_REFRESH_MS: u64 = 60_000;

    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            data: BTreeMap::new(),
            sources: Vec::new(),
            source_data: FxHashMap::default(),
            timeframe: None,
            last_refresh_request: None,
        }
    }

    /// Keep sub-5m request identity so venue adapters can combine their native
    /// 5m history with connector-archived one-minute snapshots. For larger
    /// unsupported Bybit intervals, use the nearest finer native history.
    pub(crate) fn fetch_timeframe_for(exchange: Exchange, chart: Timeframe) -> Timeframe {
        match exchange.venue() {
            Venue::Bybit => match chart {
                Timeframe::M1 | Timeframe::M3 | Timeframe::M5 => chart,
                Timeframe::M15 => Timeframe::M15,
                Timeframe::M30 => Timeframe::M30,
                Timeframe::H1 | Timeframe::H2 => Timeframe::H1,
                Timeframe::H4 | Timeframe::H12 => Timeframe::H4,
                Timeframe::D1 => Timeframe::D1,
                _ => Timeframe::M5,
            },
            Venue::Binance => chart,
            // Hyperliquid returns a current asset-context snapshot rather than
            // interval history. Keep the chart interval for request identity;
            // no resampling claim is made for that snapshot.
            Venue::Hyperliquid => chart,
            Venue::Okex | Venue::Mexc => chart,
        }
    }

    fn native_history_timeframe_for(exchange: Exchange, chart: Timeframe) -> Option<Timeframe> {
        match exchange.venue() {
            Venue::Binance => Some(match chart {
                Timeframe::M1 | Timeframe::M3 => Timeframe::M5,
                other => other,
            }),
            Venue::Bybit => Some(match chart {
                Timeframe::M1 | Timeframe::M3 | Timeframe::M5 => Timeframe::M5,
                Timeframe::M15 => Timeframe::M15,
                Timeframe::M30 => Timeframe::M30,
                Timeframe::H1 | Timeframe::H2 => Timeframe::H1,
                Timeframe::H4 | Timeframe::H12 => Timeframe::H4,
                Timeframe::D1 => Timeframe::D1,
                _ => Timeframe::M5,
            }),
            Venue::Hyperliquid => None,
            Venue::Okex | Venue::Mexc => Some(chart),
        }
    }

    fn indicator_elem<'a>(
        &'a self,
        main_chart: &'a ViewState,
        data_labels_always_visible: bool,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        if let Some(message) = self.unavailable_message(main_chart, "Open Interest") {
            return center(text(message)).into();
        }

        let (earliest, latest) = visible_range.clone().into_inner();
        if latest < earliest {
            return row![].into();
        }

        let value_label = if self.sources.len() > 1 {
            "Combined venue-reported OI"
        } else {
            "Venue-reported OI"
        };
        let tooltip = move |value: &OpenInterestCandle, _next: Option<&OpenInterestCandle>| {
            let usd = |value: f64| format!("${}", format_with_commas(value));
            let value_text = format!(
                "{value_label}: {} ({})\nO {}  H {}\nL {}  C {}",
                usd(value.close),
                abbr_large_numbers(value.close),
                usd(value.open),
                usd(value.high),
                usd(value.low),
                usd(value.close),
            );
            let change_text = change_text(value);
            let coverage = format!(
                "Sources: {}/{}",
                value.source_count, value.expected_source_count
            );
            PlotTooltip::new(format!("{value_text}\n{change_text}\n{coverage}"))
        };

        let plot = CandlePlot::new(
            |v: &OpenInterestCandle| v.open as f32,
            |v: &OpenInterestCandle| v.high as f32,
            |v: &OpenInterestCandle| v.low as f32,
            |v: &OpenInterestCandle| v.close as f32,
        )
        .with_tooltip(tooltip);

        indicator_row(
            main_chart,
            &self.cache,
            data_labels_always_visible,
            plot,
            AnySeries::forward_unix_ms(&self.data),
            visible_range,
        )
    }

    /// Fetch coverage must follow the real per-venue observations, not only
    /// drawable aggregate candles. In particular, a snapshot-only venue can
    /// keep an aggregate intentionally empty while it accumulates enough
    /// samples; using `self.data` here would strand it before the refresh path.
    fn fetch_timerange(&self, latest_kline: UnixMs) -> (UnixMs, UnixMs) {
        let Some(chart_timeframe) = self.timeframe else {
            return (latest_kline, UnixMs::ZERO);
        };
        let has_any_data = self.sources.iter().any(|source| {
            self.source_data
                .get(source)
                .is_some_and(|series| !series.is_empty())
        });
        if !has_any_data {
            return (latest_kline, UnixMs::ZERO);
        }

        // Only venues with a history endpoint can satisfy a request for older
        // coverage. Snapshot-only sources truthfully begin at collection time
        // and must not force the same impossible historical request forever.
        let historical_earliest = self
            .sources
            .iter()
            .filter(|source| {
                Self::native_history_timeframe_for(source.exchange(), chart_timeframe).is_some()
            })
            .map(|source| {
                self.source_data
                    .get(source)
                    .and_then(|series| series.first_key_value().map(|(time, _)| *time))
                    .unwrap_or(latest_kline)
            })
            .max()
            .unwrap_or(UnixMs::ZERO);

        // The common live edge is the oldest latest observation. This makes
        // a lagging venue trigger a tail refresh instead of being hidden by a
        // fresher source.
        let common_latest = self
            .sources
            .iter()
            .map(|source| {
                self.source_data
                    .get(source)
                    .and_then(|series| series.last_key_value().map(|(time, _)| *time))
                    .unwrap_or(UnixMs::ZERO)
            })
            .min()
            .unwrap_or(UnixMs::ZERO);

        (historical_earliest, common_latest)
    }

    fn rebuild_candles(&mut self) {
        self.rebuild_candles_from(None);
    }

    fn rebuild_candles_from(&mut self, changed_at: Option<UnixMs>) {
        let Some(chart_timeframe) = self.timeframe else {
            return;
        };

        // A single venue can truthfully show its first current snapshot at
        // once. A multi-venue sum starts only after every configured venue has
        // enough real observations; this avoids presenting a venue joining the
        // series as an OI increase. Values are never carried backward into time
        // before they were observed.
        const MIN_QUALIFYING_SAMPLES: usize = 5;
        // Forward carry-forward must expire: a venue that stops reporting
        // must not keep its last notional in the sum indefinitely, because
        // that presents frozen OI as live. Two buckets of silence is the
        // widest lag that still reads as "publishes slowly" rather than "went
        // quiet"; beyond it the incomplete aggregate bucket is omitted.
        const MAX_CARRY_FORWARD_BUCKETS: u64 = 2;
        let interval_ms = chart_timeframe.to_milliseconds();

        let minimum_samples = if self.sources.len() == 1 {
            1
        } else {
            MIN_QUALIFYING_SAMPLES
        };
        let qualified = self
            .sources
            .iter()
            .filter_map(|source| {
                let series = self.source_data.get(source)?;
                if series.len() < minimum_samples {
                    return None;
                }
                Some((*source, series))
            })
            .collect::<Vec<_>>();

        if qualified.len() != self.sources.len() {
            self.data.clear();
            self.clear_all_caches();
            return;
        }

        let rebuild_from =
            changed_at.map(|time| UnixMs::new(time.as_u64() / interval_ms * interval_ms));
        let mut rebuilt = if let Some(from) = rebuild_from {
            let _discarded_tail = self.data.split_off(&from);
            std::mem::take(&mut self.data)
        } else {
            BTreeMap::new()
        };
        let mut previous_close = rebuilt.last_key_value().map(|(_, candle)| candle.close);
        let recent_snapshot_cutoff = UnixMs::now()
            .as_u64()
            .saturating_sub(10 * Self::DEVELOPING_REFRESH_MS);
        let cursor_start = rebuild_from.unwrap_or(UnixMs::ZERO);
        let mut cursors = qualified
            .into_iter()
            .map(|(source, series)| SourceCursor::new(source, series, cursor_start))
            .collect::<Vec<_>>();

        while let Some(time) = cursors.iter_mut().filter_map(SourceCursor::next_time).min() {
            cursors
                .iter_mut()
                .for_each(|cursor| cursor.advance_through(time));
            let mut close = 0.0;
            let mut source_count = 0usize;
            for cursor in &cursors {
                // Use only a real observation at or before `time`. Never
                // carry a source backward before its first observation.
                let Some((sample_time, value)) = cursor.current else {
                    continue;
                };
                let staleness = time.as_u64().saturating_sub(sample_time.as_u64());
                let source_interval =
                    Self::native_history_timeframe_for(cursor.source.exchange(), chart_timeframe)
                        .unwrap_or(Timeframe::M1);
                // Venue history is interval data and can be held through its
                // native bucket. Near the live edge, however, all venues are
                // polled once per minute: never present a stopped venue as
                // live for the five-minute historical carry window.
                let carry_interval = if sample_time.as_u64() >= recent_snapshot_cutoff {
                    Self::DEVELOPING_REFRESH_MS
                } else {
                    source_interval.to_milliseconds()
                };
                if staleness > MAX_CARRY_FORWARD_BUCKETS * carry_interval {
                    continue;
                }
                source_count += 1;
                close += value;
            }
            if source_count != self.sources.len() {
                continue;
            }
            let bucket = UnixMs::new(time.as_u64() / interval_ms * interval_ms);
            let candle = rebuilt.entry(bucket).or_insert_with(|| {
                let open = previous_close.unwrap_or(close);
                OpenInterestCandle {
                    open,
                    high: open.max(close),
                    low: open.min(close),
                    close,
                    source_count,
                    expected_source_count: self.sources.len(),
                }
            });
            candle.high = candle.high.max(close);
            candle.low = candle.low.min(close);
            candle.close = close;
            candle.source_count = source_count;
            previous_close = Some(close);
        }
        self.data = rebuilt;
        self.clear_all_caches();
    }

    fn is_supported_exchange(exchange: Exchange) -> bool {
        exchange.is_perps()
            && matches!(
                exchange.venue(),
                Venue::Binance | Venue::Bybit | Venue::Hyperliquid
            )
    }

    fn is_supported_timeframe(timeframe: Timeframe) -> bool {
        Timeframe::KLINE.contains(&timeframe)
    }

    fn availability_for(basis: Basis, exchange: Exchange) -> IndicatorAvailability {
        match basis {
            Basis::Tick(_) => IndicatorAvailability::Unavailable(AvailabilityCause::Basis(basis)),
            Basis::Time(timeframe) => {
                if !Self::is_supported_exchange(exchange) {
                    IndicatorAvailability::Unavailable(AvailabilityCause::Exchange(exchange))
                } else if !Self::is_supported_timeframe(timeframe) {
                    IndicatorAvailability::Unavailable(AvailabilityCause::Timeframe(timeframe))
                } else {
                    IndicatorAvailability::Available
                }
            }
        }
    }
}

impl KlineIndicatorImpl for OpenInterestIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
    }

    fn clear_crosshair_caches(&mut self) {
        self.cache.clear_crosshair();
    }

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        data_labels_always_visible: bool,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        self.indicator_elem(chart, data_labels_always_visible, visible_range)
    }

    fn availability(&self, chart: &ViewState) -> IndicatorAvailability {
        Self::availability_for(chart.basis, chart.ticker_info.exchange())
    }

    fn fetch_range(&mut self, ctx: &FetchCtx) -> Option<FetchRange> {
        if self.timeframe != Some(ctx.timeframe) {
            self.timeframe = Some(ctx.timeframe);
            self.rebuild_candles();
        }
        let availability = Self::availability_for(
            Basis::Time(ctx.timeframe),
            ctx.main_chart.ticker_info.exchange(),
        );
        if !matches!(availability, IndicatorAvailability::Available) {
            return None;
        }

        let (oi_earliest, oi_latest) = self.fetch_timerange(ctx.kline_latest);

        if ctx.visible_earliest < oi_earliest {
            return Some(FetchRange::OpenInterest(ctx.prefetch_earliest, oi_earliest));
        }

        if oi_latest < ctx.kline_latest {
            return Some(FetchRange::OpenInterest(
                oi_latest.max(ctx.prefetch_earliest),
                ctx.kline_latest,
            ));
        }

        // Bucket coverage is up to date, but the newest candle still spans the
        // bucket that is forming right now, and venues keep revising their OI
        // inside it. Re-poll the tail on a wall-clock cadence so the
        // developing candle tracks those revisions instead of freezing at
        // whatever the bucket's first snapshot reported. The range end moves
        // with the clock so each refresh is a distinct request and cannot be
        // suppressed as an already-completed range.
        let now = UnixMs::now();
        let refresh_due = self.last_refresh_request.is_none_or(|last| {
            now.as_u64().saturating_sub(last.as_u64()) >= Self::DEVELOPING_REFRESH_MS
        });
        if refresh_due {
            self.last_refresh_request = Some(now);
            return Some(FetchRange::OpenInterest(
                oi_latest.max(ctx.prefetch_earliest),
                now,
            ));
        }

        None
    }

    fn rebuild_from_source(&mut self, _source: &PlotData<KlineDataPoint>) {
        // OI comes from network via external fetches(trade-fetch alike)
        self.clear_all_caches();
    }

    fn on_insert_klines(&mut self, _klines: &[Kline], _source: &PlotData<KlineDataPoint>) {}

    fn on_insert_trades(
        &mut self,
        _trades: &[Trade],
        _old_dp_len: usize,
        _source: &PlotData<KlineDataPoint>,
    ) {
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn on_basis_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn configure_open_interest(&mut self, sources: &[TickerInfo]) {
        self.sources = sources
            .iter()
            .copied()
            .filter(|source| Self::is_supported_exchange(source.exchange()))
            .collect();
        self.source_data
            .retain(|source, _| self.sources.contains(source));
        self.rebuild_candles();
    }

    fn open_interest_sources(&self) -> &[TickerInfo] {
        &self.sources
    }

    fn on_source_open_interest(&mut self, source: TickerInfo, values: &[exchange::OpenInterest]) {
        if !self.sources.contains(&source) {
            return;
        }
        let minimum_samples = if self.sources.len() == 1 { 1 } else { 5 };
        let already_qualified = !self.data.is_empty()
            && self.sources.iter().all(|source| {
                self.source_data
                    .get(source)
                    .is_some_and(|series| series.len() >= minimum_samples)
            });
        let series = self.source_data.entry(source).or_default();
        let mut earliest_changed = None;
        for value in values {
            if series.get(&value.time) == Some(&value.value) {
                continue;
            }
            series.insert(value.time, value.value);
            earliest_changed = Some(
                earliest_changed.map_or(value.time, |earliest: UnixMs| earliest.min(value.time)),
            );
        }
        let Some(earliest_changed) = earliest_changed else {
            return;
        };
        if already_qualified {
            self.rebuild_candles_from(Some(earliest_changed));
        } else {
            self.rebuild_candles();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{OpenInterest, Ticker};

    fn source(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    #[test]
    fn tooltip_change_uses_the_hovered_candle_body() {
        let rising = OpenInterestCandle {
            open: 100.0,
            high: 160.0,
            low: 90.0,
            close: 150.0,
            source_count: 3,
            expected_source_count: 3,
        };
        let falling = OpenInterestCandle {
            open: 150.0,
            close: 100.0,
            ..rising
        };

        assert_eq!(change_text(&rising), "Change: +$50.00");
        assert_eq!(change_text(&falling), "Change: -$50.00");
    }

    #[test]
    fn aggregates_venue_notional_and_builds_oi_candles() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, bybit, hyperliquid]);

        indicator.on_source_open_interest(
            binance,
            &[
                OpenInterest::snapshot(UnixMs::new(300_000), 7_000_000_000.0),
                OpenInterest::snapshot(UnixMs::new(600_000), 7_100_000_000.0),
                OpenInterest::snapshot(UnixMs::new(900_000), 7_050_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_200_000), 7_150_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_500_000), 7_200_000_000.0),
            ],
        );
        indicator.on_source_open_interest(
            bybit,
            &[
                OpenInterest::snapshot(UnixMs::new(300_000), 4_000_000_000.0),
                OpenInterest::snapshot(UnixMs::new(600_000), 3_900_000_000.0),
                OpenInterest::snapshot(UnixMs::new(900_000), 3_950_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_200_000), 3_850_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_500_000), 3_800_000_000.0),
            ],
        );
        indicator.on_source_open_interest(
            hyperliquid,
            &[
                OpenInterest::snapshot(UnixMs::new(600_123), 2_500_000_000.0),
                OpenInterest::snapshot(UnixMs::new(900_000), 2_600_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_200_123), 2_550_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_500_000), 2_650_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_800_123), 2_700_000_000.0),
            ],
        );

        // No aggregate is emitted before every venue has a real observation.
        assert!(!indicator.data.contains_key(&UnixMs::new(300_000)));
        let current = indicator.data[&UnixMs::new(600_000)];
        assert_eq!(current.open, 13_500_000_000.0);
        assert_eq!(current.close, 13_500_000_000.0);
        assert_eq!(current.high, 13_500_000_000.0);
        assert_eq!(current.low, 13_500_000_000.0);
        // Hyperliquid's first actual sample buckets to 600_000.
        assert_eq!(current.source_count, 3);
    }

    #[test]
    fn late_joining_source_does_not_step_the_aggregate() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, hyperliquid]);

        let oi = |time: u64, value: f64| OpenInterest::snapshot(UnixMs::new(time), value);
        // Constant per-source values so any coverage-driven step in the
        // aggregate would be visible as a candle whose close != open.
        indicator.on_source_open_interest(
            binance,
            &[
                oi(300_000, 7.0e9),
                oi(600_000, 7.0e9),
                oi(900_000, 7.0e9),
                oi(1_200_000, 7.0e9),
                oi(1_500_000, 7.0e9),
                oi(1_800_000, 7.0e9),
                oi(2_100_000, 7.0e9),
                oi(2_400_000, 7.0e9),
                oi(2_700_000, 7.0e9),
            ],
        );
        // Hyperliquid's history only begins at 1_500_000. The aggregate must
        // begin there instead of fabricating its value in earlier buckets.
        indicator.on_source_open_interest(
            hyperliquid,
            &[
                oi(1_500_000, 2.5e9),
                oi(1_800_123, 2.5e9),
                oi(2_100_123, 2.5e9),
                oi(2_400_123, 2.5e9),
                oi(2_700_123, 2.5e9),
            ],
        );

        assert_eq!(indicator.data.len(), 5);
        for candle in indicator.data.values() {
            assert_eq!(candle.close, 9_500_000_000.0);
            assert_eq!(candle.open, 9_500_000_000.0);
            assert_eq!(candle.high, 9_500_000_000.0);
            assert_eq!(candle.low, 9_500_000_000.0);
        }
        for time in [1_500_000, 1_800_000, 2_100_000, 2_400_000, 2_700_000] {
            assert_eq!(indicator.data[&UnixMs::new(time)].source_count, 2);
        }
    }

    #[test]
    fn snapshot_only_source_is_excluded_until_it_has_history() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, hyperliquid]);

        indicator.on_source_open_interest(
            binance,
            &[
                OpenInterest::snapshot(UnixMs::new(300_000), 7_000_000_000.0),
                OpenInterest::snapshot(UnixMs::new(600_000), 7_100_000_000.0),
                OpenInterest::snapshot(UnixMs::new(900_000), 7_050_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_200_000), 7_150_000_000.0),
                OpenInterest::snapshot(UnixMs::new(1_500_000), 7_200_000_000.0),
            ],
        );
        // A single live snapshot must not enter the aggregate: it would add
        // its whole notional to the newest candles and then age out again.
        indicator.on_source_open_interest(
            hyperliquid,
            &[OpenInterest::snapshot(
                UnixMs::new(600_123),
                2_500_000_000.0,
            )],
        );

        assert!(indicator.data.is_empty());
        assert_eq!(
            indicator.fetch_timerange(UnixMs::new(1_500_000)),
            (UnixMs::new(300_000), UnixMs::new(600_123))
        );
    }

    #[test]
    fn single_hyperliquid_source_displays_its_current_snapshot() {
        let hyperliquid = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M1);
        indicator.configure_open_interest(&[hyperliquid]);

        indicator.on_source_open_interest(
            hyperliquid,
            &[OpenInterest::snapshot(
                UnixMs::new(660_123),
                2_500_000_000.0,
            )],
        );

        let candle = indicator.data[&UnixMs::new(660_000)];
        assert_eq!(candle.close, 2_500_000_000.0);
        assert_eq!(candle.source_count, 1);
        assert_eq!(candle.expected_source_count, 1);
    }

    #[test]
    fn completed_history_and_live_snapshot_fill_previous_and_current_m1_candles() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M1);
        indicator.configure_open_interest(&[binance]);

        indicator.on_source_open_interest(
            binance,
            &[
                OpenInterest::completed_interval(UnixMs::new(600_000), 10.0),
                OpenInterest::snapshot(UnixMs::new(600_123), 11.0),
            ],
        );

        assert_eq!(indicator.data[&UnixMs::new(540_000)].close, 10.0);
        assert_eq!(indicator.data[&UnixMs::new(600_000)].close, 11.0);
    }

    #[test]
    fn qualified_source_is_carried_forward_when_updates_lag() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, bybit]);

        let oi = |time: u64, value: f64| OpenInterest::snapshot(UnixMs::new(time), value);
        // Bybit stops updating after 1_800_000 but has enough history to
        // qualify; within the carry-forward window its last known value is
        // used so a slow publisher does not read as an OI outflow.
        indicator.on_source_open_interest(
            bybit,
            &[
                oi(300_000, 4.4e9),
                oi(600_000, 4.3e9),
                oi(900_000, 4.2e9),
                oi(1_200_000, 4.1e9),
                oi(1_500_000, 4.0e9),
                oi(1_800_000, 3.9e9),
                oi(2_100_000, 3.85e9),
            ],
        );
        indicator.on_source_open_interest(
            binance,
            &[
                oi(300_000, 6.8e9),
                oi(600_000, 6.9e9),
                oi(900_000, 7.0e9),
                oi(1_200_000, 7.0e9),
                oi(1_500_000, 7.1e9),
                oi(1_800_000, 7.15e9),
                oi(2_100_000, 7.2e9),
                oi(2_400_000, 7.25e9),
            ],
        );

        assert_eq!(
            indicator.data[&UnixMs::new(2_100_000)].close,
            11_050_000_000.0
        );
        assert_eq!(indicator.data[&UnixMs::new(2_100_000)].source_count, 2);
        assert_eq!(
            indicator.data[&UnixMs::new(2_400_000)].close,
            11_100_000_000.0
        );
        assert_eq!(indicator.data[&UnixMs::new(2_400_000)].source_count, 2);
    }

    #[test]
    fn source_that_stops_reporting_drops_out_once_stale() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, bybit]);

        let oi = |time: u64, value: f64| OpenInterest::snapshot(UnixMs::new(time), value);
        // Bybit qualifies but goes silent after 1_500_000. Its value may be
        // carried forward across two buckets (1_800_000 and 2_100_000) but
        // must then be excluded: keeping the stale notional in the sum would
        // present frozen OI as live.
        indicator.on_source_open_interest(
            bybit,
            &[
                oi(300_000, 4.4e9),
                oi(600_000, 4.3e9),
                oi(900_000, 4.2e9),
                oi(1_200_000, 4.1e9),
                oi(1_500_000, 4.05e9),
            ],
        );
        indicator.on_source_open_interest(
            binance,
            &[
                oi(600_000, 6.9e9),
                oi(900_000, 7.0e9),
                oi(1_200_000, 7.0e9),
                oi(1_500_000, 7.1e9),
                oi(1_800_000, 7.15e9),
                oi(2_100_000, 7.2e9),
                oi(2_400_000, 7.25e9),
            ],
        );

        assert_eq!(indicator.data[&UnixMs::new(2_100_000)].source_count, 2);
        assert!(!indicator.data.contains_key(&UnixMs::new(2_400_000)));
    }

    #[test]
    fn live_aggregate_drops_a_source_after_two_missed_minute_snapshots() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M1);
        indicator.configure_open_interest(&[binance, bybit]);

        let current_minute = UnixMs::now().as_u64() / 60_000 * 60_000;
        let oi = |minutes_ago: u64, value: f64| {
            OpenInterest::snapshot(
                UnixMs::new(current_minute.saturating_sub(minutes_ago * 60_000)),
                value,
            )
        };
        indicator.on_source_open_interest(
            binance,
            &[
                oi(7, 7.0e9),
                oi(6, 7.0e9),
                oi(5, 7.0e9),
                oi(4, 7.0e9),
                oi(3, 7.0e9),
            ],
        );
        indicator.on_source_open_interest(
            bybit,
            &[
                oi(7, 3.0e9),
                oi(6, 3.0e9),
                oi(5, 3.0e9),
                oi(4, 3.0e9),
                oi(3, 3.0e9),
                oi(2, 3.0e9),
                oi(1, 3.0e9),
                oi(0, 3.0e9),
            ],
        );

        assert!(
            indicator
                .data
                .contains_key(&UnixMs::new(current_minute - 3 * 60_000))
        );
        assert!(!indicator.data.contains_key(&UnixMs::new(current_minute)));
    }

    #[test]
    fn every_kline_timeframe_uses_a_truthful_native_oi_interval() {
        for timeframe in Timeframe::KLINE {
            assert!(matches!(
                OpenInterestIndicator::availability_for(
                    Basis::Time(timeframe),
                    Exchange::BybitLinear
                ),
                IndicatorAvailability::Available
            ));
        }

        for (chart, expected) in [
            (Timeframe::M1, Timeframe::M1),
            (Timeframe::M3, Timeframe::M3),
            (Timeframe::M5, Timeframe::M5),
            (Timeframe::H2, Timeframe::H1),
            (Timeframe::H12, Timeframe::H4),
            (Timeframe::D1, Timeframe::D1),
        ] {
            assert_eq!(
                OpenInterestIndicator::fetch_timeframe_for(Exchange::BybitLinear, chart),
                expected
            );
        }
        assert_eq!(
            OpenInterestIndicator::fetch_timeframe_for(Exchange::BinanceLinear, Timeframe::H12),
            Timeframe::H12
        );
        assert_eq!(
            OpenInterestIndicator::native_history_timeframe_for(
                Exchange::BinanceLinear,
                Timeframe::M1
            ),
            Some(Timeframe::M5)
        );
    }

    #[test]
    fn native_samples_are_preserved_and_rebucketed_without_fabricated_points() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M1);
        indicator.configure_open_interest(&[binance]);

        indicator.on_source_open_interest(
            binance,
            &[
                OpenInterest::snapshot(UnixMs::new(300_000), 10.0),
                OpenInterest::snapshot(UnixMs::new(600_000), 12.0),
                OpenInterest::snapshot(UnixMs::new(900_000), 11.0),
                OpenInterest::snapshot(UnixMs::new(1_200_000), 14.0),
                OpenInterest::snapshot(UnixMs::new(1_500_000), 13.0),
            ],
        );

        assert_eq!(indicator.data.len(), 5);
        assert!(indicator.data.contains_key(&UnixMs::new(300_000)));
        assert!(!indicator.data.contains_key(&UnixMs::new(360_000)));

        indicator.timeframe = Some(Timeframe::M15);
        indicator.rebuild_candles();
        let first = indicator.data[&UnixMs::new(0)];
        assert_eq!(first.open, 10.0);
        assert_eq!(first.high, 12.0);
        assert_eq!(first.low, 10.0);
        assert_eq!(first.close, 12.0);
        let second = indicator.data[&UnixMs::new(900_000)];
        assert_eq!(second.open, 12.0);
        assert_eq!(second.high, 14.0);
        assert_eq!(second.low, 11.0);
        assert_eq!(second.close, 13.0);
    }

    #[test]
    fn supports_the_three_aggregate_venues() {
        assert!(OpenInterestIndicator::is_supported_exchange(
            Exchange::BinanceLinear
        ));
        assert!(OpenInterestIndicator::is_supported_exchange(
            Exchange::BybitLinear
        ));
        assert!(OpenInterestIndicator::is_supported_exchange(
            Exchange::HyperliquidLinear
        ));
        assert!(!OpenInterestIndicator::is_supported_exchange(
            Exchange::OkexLinear
        ));
    }

    #[test]
    fn incremental_tail_rebuild_matches_full_reference() {
        let sources = [
            source(Exchange::BinanceLinear, "BTCUSDT"),
            source(Exchange::BybitLinear, "BTCUSDT"),
        ];
        let series = |base: f64| {
            (1..=6)
                .map(|minute| {
                    OpenInterest::snapshot(UnixMs::new(minute * 60_000), base + minute as f64)
                })
                .collect::<Vec<_>>()
        };
        let mut first = series(100.0);
        let mut second = series(200.0);

        let mut incremental = OpenInterestIndicator::new();
        incremental.timeframe = Some(Timeframe::M1);
        incremental.configure_open_interest(&sources);
        incremental.on_source_open_interest(sources[0], &first);
        incremental.on_source_open_interest(sources[1], &second);

        first[3].value = 150.0;
        first.push(OpenInterest::snapshot(UnixMs::new(7 * 60_000), 160.0));
        second.push(OpenInterest::snapshot(UnixMs::new(7 * 60_000), 260.0));
        incremental.on_source_open_interest(sources[0], &[first[3], first[6]]);
        incremental.on_source_open_interest(sources[1], &[second[6]]);

        let mut reference = OpenInterestIndicator::new();
        reference.timeframe = Some(Timeframe::M1);
        reference.configure_open_interest(&sources);
        reference.on_source_open_interest(sources[0], &first);
        reference.on_source_open_interest(sources[1], &second);

        assert_eq!(incremental.data, reference.data);
    }

    #[test]
    #[ignore = "manual 90-day one-minute aggregate performance check"]
    fn benchmark_large_one_minute_aggregate_rebuild() {
        const POINTS_PER_SOURCE: u64 = 90 * 24 * 60;
        let sources = [
            source(Exchange::BinanceLinear, "BTCUSDT"),
            source(Exchange::BybitLinear, "BTCUSDT"),
            source(Exchange::HyperliquidLinear, "BTC"),
        ];
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M1);
        indicator.configure_open_interest(&sources);
        let values = (0..POINTS_PER_SOURCE)
            .map(|minute| {
                OpenInterest::snapshot(
                    UnixMs::new(minute * 60_000),
                    1_000_000_000.0 + minute as f64,
                )
            })
            .collect::<Vec<_>>();

        let initial = std::time::Instant::now();
        for source in sources {
            indicator.on_source_open_interest(source, &values);
        }
        let initial_elapsed = initial.elapsed();
        assert_eq!(indicator.data.len(), POINTS_PER_SOURCE as usize);

        let incremental = std::time::Instant::now();
        for (index, source) in sources.into_iter().enumerate() {
            indicator.on_source_open_interest(
                source,
                &[OpenInterest::snapshot(
                    UnixMs::new(POINTS_PER_SOURCE * 60_000),
                    2_000_000_000.0 + index as f64,
                )],
            );
        }
        let incremental_elapsed = incremental.elapsed();
        assert_eq!(indicator.data.len(), POINTS_PER_SOURCE as usize + 1);

        eprintln!(
            "OI aggregate: {} points, initial={initial_elapsed:?}, incremental={incremental_elapsed:?}",
            POINTS_PER_SOURCE * sources.len() as u64,
        );
    }
}
