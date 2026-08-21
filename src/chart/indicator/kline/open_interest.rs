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
}

impl OpenInterestIndicator {
    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            data: BTreeMap::new(),
            sources: Vec::new(),
            source_data: FxHashMap::default(),
            timeframe: None,
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

        let tooltip = |value: &OpenInterestCandle, next: Option<&OpenInterestCandle>| {
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
        if self.timeframe.is_none() {
            return;
        }

        // Venues without a historical OI endpoint (e.g. Hyperliquid) only
        // return a snapshot of the current value. Summing such snapshots into
        // the aggregate makes total OI jump by the whole venue notional when
        // the snapshot arrives and collapse again once it ages out of the
        // freshness window, rendering as huge fake OI candles. A source must
        // therefore accumulate real history before it may contribute, and
        // once qualified its last known value is carried forward instead of
        // being dropped when updates lag, so coverage changes never masquerade
        // as OI flows.
        const MIN_QUALIFYING_SAMPLES: usize = 5;

        let times = self
            .source_data
            .values()
            .flat_map(|series| series.keys().copied())
            .collect::<BTreeSet<_>>();
        let mut rebuilt = BTreeMap::new();
        let mut previous_close = None;

        for time in times {
            let values = self
                .sources
                .iter()
                .filter_map(|source| {
                    let series = self.source_data.get(source)?;
                    if series.len() < MIN_QUALIFYING_SAMPLES {
                        return None;
                    }
                    series.range(..=time).next_back()
                })
                .map(|(_, value)| *value)
                .collect::<Vec<_>>();
            if values.is_empty() {
                continue;
            }
            let close = values.iter().sum::<f64>();
            let open = previous_close.unwrap_or(close);
            rebuilt.insert(
                time,
                OpenInterestCandle {
                    open,
                    high: open.max(close),
                    low: open.min(close),
                    close,
                    source_count: values.len(),
                    expected_source_count: self.sources.len(),
                },
            );
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
        timeframe >= Timeframe::M5 && timeframe <= Timeframe::H4 && timeframe != Timeframe::H2
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
        let interval = self
            .timeframe
            .map(Timeframe::to_milliseconds)
            .unwrap_or_else(|| Timeframe::M5.to_milliseconds());
        let series = self.source_data.entry(source).or_default();
        for value in values {
            let bucket = value.time.as_u64() / interval * interval;
            series.insert(UnixMs::new(bucket), value.value);
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
        assert_eq!(first.close, 11_000_000_000.0);
        assert_eq!(first.source_count, 2);
        let current = indicator.data[&UnixMs::new(600_000)];
        assert_eq!(current.open, 11_000_000_000.0);
        assert_eq!(current.close, 13_500_000_000.0);
        assert_eq!(current.high, 13_500_000_000.0);
        assert_eq!(current.low, 11_000_000_000.0);
        assert_eq!(current.source_count, 3);
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
        // Bybit stops updating after 1_500_000 but has enough history to
        // qualify; its last known value must be carried forward instead of
        // dropping out of the sum.
        indicator.on_source_open_interest(
            bybit,
            &[
                oi(300_000, 4.4e9),
                oi(600_000, 4.3e9),
                oi(900_000, 4.2e9),
                oi(1_200_000, 4.1e9),
                oi(1_500_000, 4.0e9),
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
            indicator.data[&UnixMs::new(2_400_000)].close,
            11_250_000_000.0
        );
        assert_eq!(indicator.data[&UnixMs::new(2_400_000)].source_count, 2);
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
