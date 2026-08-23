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
    collections::{BTreeMap, BTreeSet},
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

impl OpenInterestIndicator {
    /// How often the still-forming bucket is re-pollen beyond bucket-boundary
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

    /// Historical OI intervals are not the same as kline intervals. Use the
    /// chart interval when the venue exposes it and otherwise fetch the
    /// nearest finer interval, which can be aggregated upward without
    /// inventing samples.
    pub(crate) fn fetch_timeframe_for(exchange: Exchange, chart: Timeframe) -> Timeframe {
        match exchange.venue() {
            Venue::Bybit => match chart {
                Timeframe::M1 | Timeframe::M3 | Timeframe::M5 => Timeframe::M5,
                Timeframe::M15 => Timeframe::M15,
                Timeframe::M30 => Timeframe::M30,
                Timeframe::H1 | Timeframe::H2 => Timeframe::H1,
                Timeframe::H4 | Timeframe::H12 => Timeframe::H4,
                Timeframe::D1 => Timeframe::D1,
                _ => Timeframe::M5,
            },
            Venue::Binance => match chart {
                Timeframe::M1 | Timeframe::M3 => Timeframe::M5,
                _ => chart,
            },
            // Hyperliquid returns a current asset-context snapshot rather than
            // interval history. Keep the chart interval for request identity;
            // no resampling claim is made for that snapshot.
            Venue::Hyperliquid => chart,
            Venue::Okex | Venue::Mexc => chart,
        }
    }

    fn source_intervals_label(&self) -> String {
        let Some(chart_timeframe) = self.timeframe else {
            return Timeframe::M5.to_string();
        };
        let intervals = self
            .sources
            .iter()
            .filter(|source| source.exchange().venue() != Venue::Hyperliquid)
            .map(|source| Self::fetch_timeframe_for(source.exchange(), chart_timeframe))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|timeframe| timeframe.to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        if intervals.is_empty() {
            "current snapshot".to_string()
        } else {
            intervals
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

        let source_intervals = self.source_intervals_label();
        let tooltip = move |value: &OpenInterestCandle, next: Option<&OpenInterestCandle>| {
            let usd = |value: f64| format!("${}", format_with_commas(value));
            let value_text = format!(
                "Aggregated OI: {} ({})\nO {}  H {}\nL {}  C {}",
                usd(value.close),
                abbr_large_numbers(value.close),
                usd(value.open),
                usd(value.high),
                usd(value.low),
                usd(value.close),
            );
            let change_text = if let Some(next_value) = next {
                let delta = next_value.close - value.close;
                let sign = if delta >= 0.0 { "+" } else { "" };
                format!("Change: {sign}${}", format_with_commas(delta.abs()))
            } else {
                "Change: N/A".to_string()
            };
            let coverage = format!(
                "Sources: {}/{}\nHistorical sampling: {}",
                value.source_count, value.expected_source_count, source_intervals
            );
            PlotTooltip::new(format!("{value_text}\n{change_text}\n{coverage}"))
        };

        let plot = CandlePlot::new(
            |v: &OpenInterestCandle| v.open as f32,
            |v: &OpenInterestCandle| v.high as f32,
            |v: &OpenInterestCandle| v.low as f32,
            |v: &OpenInterestCandle| v.close as f32,
        )
        // Open interest is snapshotted at candle open, not computed from close like regular indicators.
        // Shift left by 1 so each OI value aligns with the equivalent candle close.
        .shift(-1)
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

    // helper to compute (earliest, latest) present OI keys
    fn oi_timerange(&self, latest_kline: UnixMs) -> (UnixMs, UnixMs) {
        let mut from_time = latest_kline;
        let mut to_time = UnixMs::ZERO;

        self.data.iter().for_each(|(time, _)| {
            from_time = from_time.min(*time);
            to_time = to_time.max(*time);
        });
        (from_time, to_time)
    }

    fn rebuild_candles(&mut self) {
        let Some(chart_timeframe) = self.timeframe else {
            return;
        };

        // Venues without a historical OI endpoint (e.g. Hyperliquid) only
        // return a snapshot of the current value, and some venues cap how far
        // back their OI history endpoint paginates. Summing such a source into
        // the aggregate only from its first sample makes total OI jump by the
        // whole venue notional mid-series, masquerading as an OI flow. A
        // source must therefore accumulate real history before it may
        // contribute, and once qualified its nearest known value is used in
        // both directions — carried forward when updates lag and carried back
        // across candles that predate its first sample — so coverage changes
        // shift the level once at most and never render as fake OI candles.
        const MIN_QUALIFYING_SAMPLES: usize = 5;
        // Forward carry-forward must expire: a venue that stops reporting
        // must not keep its last notional in the sum indefinitely, because
        // that presents frozen OI as live. Two buckets of silence is the
        // widest lag that still reads as "publishes slowly" rather than "went
        // quiet"; beyond it the source drops out and the coverage count says
        // so.
        const MAX_CARRY_FORWARD_BUCKETS: u64 = 2;
        let interval_ms = chart_timeframe.to_milliseconds();

        let qualified = self
            .sources
            .iter()
            .filter_map(|source| {
                let series = self.source_data.get(source)?;
                if series.len() < MIN_QUALIFYING_SAMPLES {
                    return None;
                }
                Some((*source, series))
            })
            .collect::<Vec<_>>();

        let times = qualified
            .iter()
            .flat_map(|(_, series)| series.keys().copied())
            .collect::<BTreeSet<_>>();
        let mut rebuilt = BTreeMap::new();
        let mut previous_close = None;

        for time in times {
            let mut close = 0.0;
            let mut source_count = 0usize;
            for (source, series) in &qualified {
                // Last known value at or before `time`, else the earliest
                // known value carried back across preceding candles.
                let value = match series.range(..=time).next_back() {
                    Some((sample_time, value)) => {
                        let staleness = time.as_u64().saturating_sub(sample_time.as_u64());
                        let source_interval =
                            Self::fetch_timeframe_for(source.exchange(), chart_timeframe);
                        if staleness > MAX_CARRY_FORWARD_BUCKETS * source_interval.to_milliseconds()
                        {
                            continue;
                        }
                        source_count += 1;
                        *value
                    }
                    None => *series.values().next().expect("qualified series non-empty"),
                };
                close += value;
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

        let (oi_earliest, oi_latest) = self.oi_timerange(ctx.kline_latest);

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
        let series = self.source_data.entry(source).or_default();
        for value in values {
            series.insert(value.time, value.value);
        }
        self.rebuild_candles();
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
                OpenInterest {
                    time: UnixMs::new(300_000),
                    value: 7_000_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(600_000),
                    value: 7_100_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(900_000),
                    value: 7_050_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_200_000),
                    value: 7_150_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_500_000),
                    value: 7_200_000_000.0,
                },
            ],
        );
        indicator.on_source_open_interest(
            bybit,
            &[
                OpenInterest {
                    time: UnixMs::new(300_000),
                    value: 4_000_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(600_000),
                    value: 3_900_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(900_000),
                    value: 3_950_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_200_000),
                    value: 3_850_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_500_000),
                    value: 3_800_000_000.0,
                },
            ],
        );
        indicator.on_source_open_interest(
            hyperliquid,
            &[
                OpenInterest {
                    time: UnixMs::new(600_123),
                    value: 2_500_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(900_000),
                    value: 2_600_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_200_123),
                    value: 2_550_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_500_000),
                    value: 2_650_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_800_123),
                    value: 2_700_000_000.0,
                },
            ],
        );

        let first = indicator.data[&UnixMs::new(300_000)];
        // Hyperliquid has no sample this early; its first known value is
        // carried back so the aggregate level is continuous across coverage.
        assert_eq!(first.close, 13_500_000_000.0);
        assert_eq!(first.source_count, 2);
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

        let oi = |time: u64, value: f64| OpenInterest {
            time: UnixMs::new(time),
            value,
        };
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
        // Hyperliquid's history only begins at 1_500_000 (snapshot venue or
        // short history window). Once it qualifies it must not add a fake
        // +2.5b step at that point: its first value is carried back instead.
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

        assert_eq!(indicator.data.len(), 9);
        for candle in indicator.data.values() {
            assert_eq!(candle.close, 9_500_000_000.0);
            assert_eq!(candle.open, 9_500_000_000.0);
            assert_eq!(candle.high, 9_500_000_000.0);
            assert_eq!(candle.low, 9_500_000_000.0);
        }
        // Coverage stays honest even though the level is continuous.
        for time in [300_000, 600_000, 900_000, 1_200_000] {
            assert_eq!(indicator.data[&UnixMs::new(time)].source_count, 1);
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
                OpenInterest {
                    time: UnixMs::new(300_000),
                    value: 7_000_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(600_000),
                    value: 7_100_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(900_000),
                    value: 7_050_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_200_000),
                    value: 7_150_000_000.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_500_000),
                    value: 7_200_000_000.0,
                },
            ],
        );
        // A single live snapshot must not enter the aggregate: it would add
        // its whole notional to the newest candles and then age out again.
        indicator.on_source_open_interest(
            hyperliquid,
            &[OpenInterest {
                time: UnixMs::new(600_123),
                value: 2_500_000_000.0,
            }],
        );

        assert_eq!(indicator.data.len(), 5);
        for candle in indicator.data.values() {
            assert_eq!(candle.source_count, 1);
            assert_eq!(candle.expected_source_count, 2);
            assert!(candle.close < 8_000_000_000.0);
        }
    }

    #[test]
    fn qualified_source_is_carried_forward_when_updates_lag() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = OpenInterestIndicator::new();
        indicator.timeframe = Some(Timeframe::M5);
        indicator.configure_open_interest(&[binance, bybit]);

        let oi = |time: u64, value: f64| OpenInterest {
            time: UnixMs::new(time),
            value,
        };
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

        let oi = |time: u64, value: f64| OpenInterest {
            time: UnixMs::new(time),
            value,
        };
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
        assert_eq!(
            indicator.data[&UnixMs::new(2_400_000)].close,
            7_250_000_000.0
        );
        assert_eq!(indicator.data[&UnixMs::new(2_400_000)].source_count, 1);
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
            (Timeframe::M1, Timeframe::M5),
            (Timeframe::M3, Timeframe::M5),
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
                OpenInterest {
                    time: UnixMs::new(300_000),
                    value: 10.0,
                },
                OpenInterest {
                    time: UnixMs::new(600_000),
                    value: 12.0,
                },
                OpenInterest {
                    time: UnixMs::new(900_000),
                    value: 11.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_200_000),
                    value: 14.0,
                },
                OpenInterest {
                    time: UnixMs::new(1_500_000),
                    value: 13.0,
                },
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
}
