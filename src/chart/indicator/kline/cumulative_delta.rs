use super::KlineIndicatorImpl;
use super::footprint_history::{DAY_MS, DayBookRetain, FootprintHistoryIndicator, day_start};
use crate::chart::{
    Basis, Caches, Message, ViewState,
    indicator::{
        indicator_row,
        kline::{AvailabilityCause, IndicatorAvailability},
        plot::{AnySeries, PlotTooltip, candle::CandlePlot},
    },
};

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use data::chart::{PlotData, kline::KlineDataPoint};
use data::util::format_with_commas;
use exchange::{TickerInfo, Timeframe, Trade, UnixMs};

use iced::widget::{center, text};

/// Resolution of the shared UTC-day trade books backing this indicator.
const FIVE_MIN_MS: u64 = 5 * 60 * 1_000;
/// UTC days of multi-venue trade history retained in the running sum. Days
/// are slimmed to their five-minute delta buckets once persisted, so the
/// window costs memory linear in buckets, not price levels.
pub(crate) const LOOKBACK_DAYS: usize = 90;
/// UTC days the chart actively backfills on this indicator's behalf.
///
/// Retention spans [`LOOKBACK_DAYS`], but eagerly pulling months of
/// multi-venue tick trades through rate-limited REST starves every other
/// fetch — including the chart's own kline requests, which is how candle
/// panes ended up blank while trade backfills ran for hours. Days beyond
/// this horizon therefore come from disk caches (shared with Footprint
/// History / Daily Delta and previous sessions) instead of eager fetching;
/// the running sum simply starts at the oldest day actually available.
pub(crate) const FETCH_LOOKBACK_DAYS: u16 = 7;
/// Upper bound for externally configured lookbacks (Daily Delta-style
/// settings), kept well inside the shared day books' retention limit.
const MAX_CONFIGURED_LOOKBACK_DAYS: u16 = 30;

/// One aggregated CVD bar. Values are USD notional (buy minus sell), matching
/// the shared day books used by Footprint History and Daily Delta.
#[derive(Debug, Clone, Copy)]
pub struct DeltaCandle {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    /// Buy - sell notional traded inside this bucket.
    delta: f64,
}

pub struct CumulativeDeltaIndicator {
    cache: Caches,
    inner: FootprintHistoryIndicator,
    lookback_days: usize,
    /// Chart bucket width in milliseconds. `None` on tick basis, where
    /// time-keyed aggregate candles cannot be placed.
    interval_ms: Option<u64>,
    merged: MergedDays,
    candles: BTreeMap<UnixMs, DeltaCandle>,
    has_data: bool,
}

impl CumulativeDeltaIndicator {
    pub fn new() -> Self {
        let mut inner = FootprintHistoryIndicator::new();
        inner.set_lookback_days(LOOKBACK_DAYS as u16);
        inner.set_day_book_retain(DayBookRetain::DELTAS_WITHOUT_PRINTS);
        Self {
            cache: Caches::default(),
            inner,
            lookback_days: LOOKBACK_DAYS,
            interval_ms: None,
            merged: MergedDays::new(LOOKBACK_DAYS),
            candles: BTreeMap::new(),
            has_data: false,
        }
    }

    fn availability_for(basis: Basis) -> IndicatorAvailability {
        match basis {
            Basis::Tick(_) => IndicatorAvailability::Unavailable(AvailabilityCause::Basis(basis)),
            Basis::Time(timeframe) => {
                // The day book only resolves five-minute deltas, so finer chart
                // buckets cannot be filled honestly.
                if timeframe < Timeframe::M5 {
                    IndicatorAvailability::Unavailable(AvailabilityCause::Timeframe(timeframe))
                } else {
                    IndicatorAvailability::Available
                }
            }
        }
    }

    fn indicator_elem<'a>(
        &'a self,
        main_chart: &'a ViewState,
        data_labels_always_visible: bool,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        if let Some(message) = self.unavailable_message(main_chart, "CVD") {
            return center(text(message)).into();
        }

        let exact = |value: f64| format!("${}", format_with_commas(value));
        let signed_exact = move |value: f64| {
            format!(
                "{}{}",
                if value >= 0.0 { "+" } else { "-" },
                exact(value.abs())
            )
        };
        let tooltip = move |candle: &DeltaCandle, _next: Option<&DeltaCandle>| {
            PlotTooltip::new(format!(
                "CVD: {}\nO {}  H {}\nL {}  C {}\nΔ {}",
                exact(candle.close),
                exact(candle.open),
                exact(candle.high),
                exact(candle.low),
                exact(candle.close),
                signed_exact(candle.delta),
            ))
        };

        let plot = CandlePlot::new(
            |candle: &DeltaCandle| candle.open as f32,
            |candle: &DeltaCandle| candle.high as f32,
            |candle: &DeltaCandle| candle.low as f32,
            |candle: &DeltaCandle| candle.close as f32,
        )
        .with_tooltip(tooltip);

        indicator_row(
            main_chart,
            &self.cache,
            data_labels_always_visible,
            plot,
            AnySeries::forward_unix_ms(&self.candles),
            visible_range,
        )
    }

    fn refresh_merged(&mut self) {
        self.merged.refresh(&self.inner, self.lookback_days);
        self.assemble_candles();
    }

    /// Fold the cached per-day venue-merged deltas into running CVD candles.
    fn assemble_candles(&mut self) {
        let Some(interval_ms) = self.interval_ms.filter(|ms| *ms >= FIVE_MIN_MS) else {
            self.candles.clear();
            return;
        };

        let mut candles = BTreeMap::new();
        let mut cumulative = 0.0_f64;
        for day in &self.merged.days {
            for (bucket_start, delta) in &day.deltas {
                let bucket = bucket_start / interval_ms * interval_ms;
                let after = cumulative + delta;
                let candle = candles.entry(UnixMs::new(bucket)).or_insert(DeltaCandle {
                    open: cumulative,
                    high: cumulative.max(after),
                    low: cumulative.min(after),
                    close: after,
                    delta: 0.0,
                });
                candle.close = after;
                candle.high = candle.high.max(after);
                candle.low = candle.low.min(after);
                candle.delta += delta;
                cumulative = after;
            }
        }
        self.has_data = !candles.is_empty();
        self.candles = candles;
        self.cache.clear_all();
    }
}

impl KlineIndicatorImpl for CumulativeDeltaIndicator {
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
        match Self::availability_for(chart.basis) {
            IndicatorAvailability::Available if !self.has_data => IndicatorAvailability::Unknown,
            available => available,
        }
    }

    fn rebuild_from_source(&mut self, source: &PlotData<KlineDataPoint>) {
        self.interval_ms = match source {
            PlotData::TimeBased(series) => Some(series.interval.to_milliseconds()),
            PlotData::TickBased(_) => None,
        };
        self.assemble_candles();
    }

    fn on_ticksize_change(&mut self, _source: &PlotData<KlineDataPoint>) {}

    fn on_basis_change(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild_from_source(source);
    }

    fn configure_footprint_history(&mut self, sources: &[TickerInfo], aggregate: bool) {
        self.inner.configure_footprint_history(sources, aggregate);
        self.inner.set_lookback_days(self.lookback_days as u16);
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn set_trade_history_lookback(&mut self, days: u16) {
        let days = days.clamp(1, MAX_CONFIGURED_LOOKBACK_DAYS);
        self.lookback_days = usize::from(days);
        self.inner.set_lookback_days(days);
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn reset_trade_history_backfill(&mut self) {
        self.inner.reset_trade_history_backfill();
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn prepare_footprint_history(&mut self, source: TickerInfo, cutoff: UnixMs) {
        self.inner.prepare_footprint_history(source, cutoff);
    }

    fn load_cached_footprint_day(&mut self, source: TickerInfo, day: UnixMs) -> Option<UnixMs> {
        let loaded = self.inner.load_cached_footprint_day(source, day);
        if let Some(covered_through) = loaded.as_ref() {
            if FootprintHistoryIndicator::day_is_complete(day, *covered_through) {
                // Complete day from disk: level detail is not needed for the
                // running sum and would otherwise accumulate per venue.
                self.inner.slim_day_levels(day.as_u64());
            }
            self.merged.mark_dirty_from(day_start(day));
            self.refresh_merged();
        }
        loaded
    }

    fn persist_cached_footprint_day(
        &mut self,
        source: TickerInfo,
        day_start: UnixMs,
        covered_through: UnixMs,
    ) {
        self.inner
            .persist_cached_footprint_day(source, day_start, covered_through);
        if FootprintHistoryIndicator::day_is_complete(day_start, covered_through) {
            // Persisted to the shared disk cache: drop level detail so a long
            // lookback does not retain per-venue level maps in memory.
            self.inner.slim_day_levels(day_start.as_u64());
        }
    }

    fn on_source_trades(&mut self, source: TickerInfo, trades: &[Trade], historical: bool) {
        self.inner.on_source_trades(source, trades, historical);
        if let Some(earliest) = trades.iter().map(|trade| trade.time.as_u64()).min() {
            self.merged
                .mark_dirty_from(day_start(UnixMs::new(earliest)));
            self.refresh_merged();
        }
    }

    fn stage_source_trades(&mut self, req_id: uuid::Uuid, source: TickerInfo, trades: &[Trade]) {
        self.inner.stage_source_trades(req_id, source, trades);
    }

    fn commit_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.inner.commit_staged_source_trades(req_id);
        self.merged.invalidate_all();
        self.refresh_merged();
    }

    fn discard_staged_source_trades(&mut self, req_id: uuid::Uuid) {
        self.inner.discard_staged_source_trades(req_id);
    }
}

struct MergedDay {
    day_ts: u64,
    deltas: BTreeMap<u64, f64>,
}

/// Per-day cache of venue-merged five-minute deltas, so panning/zooming and
/// live ticks never re-absorb whole days of trades per frame.
struct MergedDays {
    generation: u64,
    built: Option<(u64, usize, u64)>,
    dirty_from_day: Option<u64>,
    days: Vec<MergedDay>,
}

impl MergedDays {
    fn new(_lookback_days: usize) -> Self {
        Self {
            generation: 0,
            built: None,
            dirty_from_day: None,
            days: Vec::new(),
        }
    }

    fn invalidate_all(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn mark_dirty_from(&mut self, day_ts: u64) {
        self.dirty_from_day = Some(
            self.dirty_from_day
                .map_or(day_ts, |current| current.min(day_ts)),
        );
    }

    fn refresh(&mut self, inner: &FootprintHistoryIndicator, lookback_days: usize) {
        let today = day_start(UnixMs::now());
        let lookback_days = lookback_days.max(1);
        let needs_full_rebuild = self.built.is_none_or(|built| {
            let (generation, days, built_today) = built;
            generation != self.generation || days != lookback_days || built_today != today
        });
        if needs_full_rebuild {
            self.built = Some((self.generation, lookback_days, today));
            self.dirty_from_day = None;
            self.days = (0..lookback_days)
                .rev()
                .map(|offset| {
                    let day_ts = today.saturating_sub(offset as u64 * DAY_MS);
                    MergedDay {
                        day_ts,
                        deltas: merged_day_deltas(inner, day_ts),
                    }
                })
                .collect();
        } else if let Some(from) = self.dirty_from_day.take() {
            for day in &mut self.days {
                if day.day_ts >= from {
                    day.deltas = merged_day_deltas(inner, day.day_ts);
                }
            }
        }
    }
}

fn merged_day_deltas(inner: &FootprintHistoryIndicator, day_ts: u64) -> BTreeMap<u64, f64> {
    inner.merged_five_min_deltas(day_ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, adapter::Exchange, unit::Price, unit::Qty};

    const FIVE_MIN: u64 = 5 * 60 * 1_000;
    const FIFTEEN_MIN: u64 = 15 * 60 * 1_000;

    fn source(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    fn trade(time: u64, price: f64, qty: f64, is_sell: bool) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(qty),
            is_sell,
        }
    }

    #[test]
    fn aggregates_venues_into_running_cvd_candles() {
        let binance = source(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = source(Exchange::BybitLinear, "BTCUSDT");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[binance, bybit], true);
        indicator.interval_ms = Some(FIFTEEN_MIN);

        let today = day_start(UnixMs::now());
        // Buy and sell sit in different 5m day-book buckets within the first
        // M15 bucket, so the candle records the intra-bucket high before
        // fading.
        indicator.on_source_trades(binance, &[trade(today + 60_000, 10.0, 100.0, false)], true);
        indicator.on_source_trades(
            bybit,
            &[trade(today + FIVE_MIN + 30_000, 10.0, 50.0, true)],
            true,
        );
        // Next M15 bucket, next day-book bucket.
        indicator.on_source_trades(
            binance,
            &[trade(
                today + 2 * FIVE_MIN + 30_000 + FIFTEEN_MIN,
                10.0,
                200.0,
                false,
            )],
            true,
        );

        assert!(indicator.has_data);
        let first = indicator.candles[&UnixMs::new(today)];
        assert_eq!(first.open, 0.0);
        assert_eq!(first.high, 1_000.0);
        assert_eq!(first.low, 0.0);
        assert_eq!(first.close, 500.0);
        assert_eq!(first.delta, 500.0);

        let second = indicator.candles[&UnixMs::new(today + FIFTEEN_MIN)];
        assert_eq!(second.open, 500.0);
        assert_eq!(second.high, 2_500.0);
        assert_eq!(second.low, 500.0);
        assert_eq!(second.close, 2_500.0);
        assert_eq!(second.delta, 2_000.0);
        assert_eq!(indicator.candles.len(), 2);
    }

    #[test]
    fn single_source_cvd_still_builds_without_aggregation() {
        let only = source(Exchange::HyperliquidLinear, "BTC");
        let mut indicator = CumulativeDeltaIndicator::new();
        indicator.configure_footprint_history(&[only], false);
        indicator.interval_ms = Some(Timeframe::M5.to_milliseconds());

        let today = day_start(UnixMs::now());
        indicator.on_source_trades(only, &[trade(today + 10_000, 5.0, 2.0, true)], true);

        let first = indicator.candles[&UnixMs::new(today)];
        assert_eq!(first.close, -10.0);
        assert_eq!(first.delta, -10.0);
    }

    #[test]
    fn sub_five_minute_and_tick_basis_are_unavailable() {
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::M1)),
            IndicatorAvailability::Unavailable(AvailabilityCause::Timeframe(_))
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::M5)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Time(Timeframe::H4)),
            IndicatorAvailability::Available
        ));
        assert!(matches!(
            CumulativeDeltaIndicator::availability_for(Basis::Tick(data::aggr::TickCount(100))),
            IndicatorAvailability::Unavailable(AvailabilityCause::Basis(_))
        ));
    }
}
