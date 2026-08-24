use std::collections::BTreeMap;

use crate::chart::Basis;
use crate::chart::heatmap::HeatmapDataPoint;
use crate::chart::kline::{ClusterKind, KlineDataPoint, KlineTrades, NPoc};

use exchange::unit::{Price, PriceStep, Qty};
use exchange::{Kline, Timeframe, Trade, UnixMs, Volume};
use rustc_hash::FxHashSet;

pub trait DataPoint {
    fn add_trade(&mut self, trade: &Trade, step: PriceStep);

    fn clear_trades(&mut self);

    fn last_trade_time(&self) -> Option<UnixMs>;

    fn first_trade_time(&self) -> Option<UnixMs>;

    fn last_price(&self) -> Price;

    fn kline(&self) -> Option<&Kline>;

    fn value_high(&self) -> Price;

    fn value_low(&self) -> Price;
}

pub struct TimeSeries<D: DataPoint> {
    pub datapoints: BTreeMap<UnixMs, D>,
    pub interval: Timeframe,
    pub tick_size: PriceStep,
}

impl<D: DataPoint> TimeSeries<D> {
    pub fn base_price(&self) -> Price {
        self.datapoints
            .values()
            .last()
            .map_or(Price::from_f32(0.0), DataPoint::last_price)
    }

    pub fn latest_timestamp(&self) -> Option<UnixMs> {
        self.datapoints.keys().last().copied()
    }

    pub fn latest_kline(&self) -> Option<&Kline> {
        self.datapoints.values().last().and_then(|dp| dp.kline())
    }

    pub fn price_scale(&self, lookback: usize) -> (Price, Price) {
        let mut iter = self.datapoints.iter().rev().take(lookback);

        if let Some((_, first)) = iter.next() {
            let mut high = first.value_high();
            let mut low = first.value_low();

            for (_, dp) in iter {
                let value_high = dp.value_high();
                let value_low = dp.value_low();
                if value_high > high {
                    high = value_high;
                }
                if value_low < low {
                    low = value_low;
                }
            }

            (high, low)
        } else {
            (Price::from_f32(0.0), Price::from_f32(0.0))
        }
    }

    pub fn volume_data<'a>(&'a self) -> BTreeMap<UnixMs, exchange::Volume>
    where
        BTreeMap<UnixMs, exchange::Volume>: From<&'a TimeSeries<D>>,
    {
        self.into()
    }

    pub fn timerange(&self) -> (UnixMs, UnixMs) {
        let earliest = self
            .datapoints
            .keys()
            .next()
            .copied()
            .unwrap_or(UnixMs::ZERO);
        let latest = self
            .datapoints
            .keys()
            .last()
            .copied()
            .unwrap_or(UnixMs::ZERO);

        (earliest, latest)
    }

    pub fn min_max_price_in_range_prices(
        &self,
        earliest: UnixMs,
        latest: UnixMs,
    ) -> Option<(Price, Price)> {
        let mut it = self.datapoints.range(earliest..=latest);

        let (_, first) = it.next()?;
        let mut min_price = first.value_low();
        let mut max_price = first.value_high();

        for (_, dp) in it {
            let low = dp.value_low();
            let high = dp.value_high();
            if low < min_price {
                min_price = low;
            }
            if high > max_price {
                max_price = high;
            }
        }

        Some((min_price, max_price))
    }

    pub fn min_max_price_in_range(&self, earliest: UnixMs, latest: UnixMs) -> Option<(f32, f32)> {
        self.min_max_price_in_range_prices(earliest, latest)
            .map(|(min_p, max_p)| (min_p.to_f32_lossy(), max_p.to_f32_lossy()))
    }

    /// Ensures a datapoint bucket exists at `rounded_t` and ingests all trades into it.
    pub fn ingest_trades_bucket(&mut self, rounded_t: UnixMs, trades: &[Trade], step: PriceStep)
    where
        D: Default,
    {
        let bucket = self.datapoints.entry(rounded_t).or_default();

        for trade in trades {
            bucket.add_trade(trade, step);
        }
    }

    pub fn clear_trades(&mut self) {
        for data_point in self.datapoints.values_mut() {
            data_point.clear_trades();
        }
    }

    fn align_down_to_phase(time: UnixMs, phase: UnixMs, interval: u64) -> UnixMs {
        if time >= phase {
            let t = time.as_u64();
            let p = phase.as_u64();
            UnixMs::new(t.saturating_sub((t - p) % interval))
        } else {
            phase
        }
    }

    fn check_kline_integrity_range(
        &self,
        earliest: UnixMs,
        latest: UnixMs,
        interval: u64,
    ) -> Option<Vec<UnixMs>> {
        let mut time = earliest;
        let mut missing_count = 0;

        while time < latest {
            if !self.datapoints.contains_key(&time) {
                missing_count += 1;
                break;
            }
            time = time.saturating_add(interval);
        }

        if missing_count > 0 {
            let mut missing_keys =
                Vec::with_capacity(((latest.as_u64() - earliest.as_u64()) / interval) as usize);
            let mut time = earliest;

            while time < latest {
                if !self.datapoints.contains_key(&time) {
                    missing_keys.push(time);
                }
                time = time.saturating_add(interval);
            }

            log::debug!(
                "Integrity check failed: missing {} klines",
                missing_keys.len()
            );
            return Some(missing_keys);
        }

        None
    }

    pub fn check_kline_integrity(&self, earliest: UnixMs, latest: UnixMs) -> Option<Vec<UnixMs>> {
        if self.datapoints.is_empty() {
            return None;
        }

        let interval = self.interval.to_milliseconds();
        if interval == 0 {
            return None;
        }

        let (series_earliest, series_latest) = self.timerange();
        let phase = UnixMs::new(series_earliest.as_u64() % interval);

        let check_earliest =
            Self::align_down_to_phase(earliest.max(series_earliest), phase, interval)
                .max(series_earliest);
        let check_latest = Self::align_down_to_phase(latest.min(series_latest), phase, interval)
            .min(series_latest);

        if check_earliest < check_latest {
            self.check_kline_integrity_range(check_earliest, check_latest, interval)
        } else {
            None
        }
    }
}

impl TimeSeries<KlineDataPoint> {
    pub fn new(interval: Timeframe, tick_size: PriceStep, klines: &[Kline]) -> Self {
        let mut timeseries = Self {
            datapoints: BTreeMap::new(),
            interval,
            tick_size,
        };

        timeseries.insert_klines(klines);
        timeseries
    }

    pub fn with_trades(&self, trades: &[Trade]) -> TimeSeries<KlineDataPoint> {
        let mut new_series = Self {
            datapoints: self.datapoints.clone(),
            interval: self.interval,
            tick_size: self.tick_size,
        };

        new_series.insert_trades_or_create_bucket(trades);
        new_series
    }

    pub fn insert_klines(&mut self, klines: &[Kline]) {
        for kline in klines {
            let entry = self
                .datapoints
                .entry(kline.time)
                .or_insert_with(|| KlineDataPoint {
                    kline: *kline,
                    footprint: KlineTrades::new(),
                });

            entry.kline = *kline;
        }

        self.update_poc_status();
    }

    pub fn insert_trades_or_create_bucket(&mut self, buffer: &[Trade]) {
        if buffer.is_empty() {
            return;
        }
        let mut updated_times = Vec::new();

        buffer.iter().for_each(|trade| {
            let rounded_time = trade.time.floor_to(self.interval);

            if !updated_times.contains(&rounded_time) {
                updated_times.push(rounded_time);
            }

            let entry = self
                .datapoints
                .entry(rounded_time)
                .or_insert_with(|| KlineDataPoint {
                    kline: Kline {
                        time: rounded_time,
                        open: trade.price,
                        high: trade.price,
                        low: trade.price,
                        close: trade.price,
                        volume: Volume::empty_buy_sell(),
                    },
                    footprint: KlineTrades::new(),
                });

            entry.add_trade(trade, self.tick_size);
        });

        for time in updated_times {
            if let Some(data_point) = self.datapoints.get_mut(&time) {
                data_point.calculate_poc();
            }
        }
    }

    pub fn insert_trades_existing_buckets(&mut self, buffer: &[Trade]) {
        if buffer.is_empty() {
            return;
        }
        let mut updated_times: Vec<UnixMs> = Vec::new();

        for trade in buffer {
            let rounded_time = trade.time.floor_to(self.interval);

            if let Some(entry) = self.datapoints.get_mut(&rounded_time) {
                if !updated_times.contains(&rounded_time) {
                    updated_times.push(rounded_time);
                }
                entry.add_trade(trade, self.tick_size);
            }
        }

        for time in updated_times {
            if let Some(data_point) = self.datapoints.get_mut(&time) {
                data_point.calculate_poc();
            }
        }
    }

    /// Apply `buffer` only to candles that still have an empty footprint.
    ///
    /// Historical kline pages used to replay the whole live buffer onto every
    /// existing bucket, so already-filled cells grew a second copy of every
    /// print. New empty slots still get a one-shot backfill from the buffer.
    pub fn fill_empty_footprints_from_trades(&mut self, buffer: &[Trade]) {
        if buffer.is_empty() {
            return;
        }

        let empty_times: FxHashSet<UnixMs> = self
            .datapoints
            .iter()
            .filter(|(_, dp)| dp.footprint.trades.is_empty())
            .map(|(time, _)| *time)
            .collect();
        if empty_times.is_empty() {
            return;
        }

        let mut updated_times = FxHashSet::default();
        for trade in buffer {
            let rounded_time = trade.time.floor_to(self.interval);
            if !empty_times.contains(&rounded_time) {
                continue;
            }
            let Some(entry) = self.datapoints.get_mut(&rounded_time) else {
                continue;
            };
            updated_times.insert(rounded_time);
            entry.add_trade(trade, self.tick_size);
        }

        for time in updated_times {
            if let Some(data_point) = self.datapoints.get_mut(&time) {
                data_point.calculate_poc();
            }
        }
    }

    /// The footprint is stored at a fixed base price step (see [`Self::new`]).
    /// Display grouping happens at draw time via `KlineTrades::grouped_to_step`,
    /// so changing the tick multiplier never needs to touch stored levels and
    /// switching between multipliers is lossless in both directions.
    pub fn storage_tick_size(&self) -> PriceStep {
        self.tick_size
    }

    pub fn update_poc_status(&mut self) {
        let updates = self
            .datapoints
            .iter()
            .filter_map(|(&time, dp)| dp.poc_price().map(|price| (time, price)))
            .collect::<Vec<_>>();

        for (current_time, poc_price) in updates {
            let mut npoc = NPoc::default();

            for (&next_time, next_dp) in self.datapoints.range(current_time.saturating_add(1)..) {
                let next_dp_low = next_dp.kline.low.round_to_side_step(true, self.tick_size);
                let next_dp_high = next_dp.kline.high.round_to_side_step(false, self.tick_size);

                if next_dp_low <= poc_price && next_dp_high >= poc_price {
                    npoc.filled(next_time.as_u64());
                    break;
                } else {
                    npoc.unfilled();
                }
            }

            if let Some(data_point) = self.datapoints.get_mut(&current_time) {
                data_point.set_poc_status(npoc);
            }
        }
    }

    pub fn min_max_footprint_price_in_range(
        &self,
        earliest: UnixMs,
        latest: UnixMs,
    ) -> Option<(Price, Price)> {
        if latest < earliest {
            return None;
        }

        let mut min_price: Option<Price> = None;
        let mut max_price: Option<Price> = None;

        let mut track_price = |price: Price| {
            min_price = Some(match min_price {
                Some(current) => current.min(price),
                None => price,
            });
            max_price = Some(match max_price {
                Some(current) => current.max(price),
                None => price,
            });
        };

        self.datapoints
            .range(earliest..=latest)
            .for_each(|(_, dp)| {
                track_price(dp.kline.low);
                track_price(dp.kline.high);

                for price in dp.footprint.trades.keys() {
                    track_price(*price);
                }
            });

        match (min_price, max_price) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        }
    }

    pub fn suggest_trade_fetch_range(
        &self,
        visible_earliest: UnixMs,
        visible_latest: UnixMs,
    ) -> Option<(UnixMs, UnixMs)> {
        if self.datapoints.is_empty() {
            return None;
        }

        self.find_trade_gap(visible_earliest, visible_latest)
            .and_then(|(last_t_before_gap, first_t_after_gap)| {
                if last_t_before_gap.is_none() && first_t_after_gap.is_none() {
                    // A freshly enabled trade-backed overlay (such as VPVR)
                    // starts with kline buckets but no footprints at all. Do
                    // not wait for the first live print before requesting the
                    // visible history: there is no trade boundary to infer yet.
                    let interval_ms = self.interval.to_milliseconds();
                    let first = self
                        .datapoints
                        .range(visible_earliest.floor_to(self.interval)..=visible_latest)
                        .next()
                        .map(|(time, _)| *time)?;
                    let last = self
                        .datapoints
                        .range(first..=visible_latest)
                        .next_back()
                        .map(|(time, _)| *time)?;
                    let end = last.saturating_add(interval_ms.saturating_sub(1));
                    return (first < end).then_some((first, end));
                }
                let (data_earliest, data_latest) = self.timerange();

                let gap_start = last_t_before_gap.map_or(data_earliest, |t| t.saturating_add(1));

                // Round down to the nearest kline boundary so the first
                // bucket is fully covered.
                let fetch_from = gap_start
                    .max(visible_earliest)
                    .floor_to(self.interval)
                    .max(gap_start);

                // When we know where the next trade sits, fetch the entire
                // gap in one shot instead of truncating at `visible_latest`
                // - otherwise fast scrolling leaves a trailing gap that
                // must be back-filled on the next scroll.  When there is no
                // trade after the gap (`None`) we still stop at the visible
                // edge to avoid an unbounded fetch.
                let fetch_to = match first_t_after_gap {
                    Some(t) => t.saturating_sub(1),
                    None => data_latest.min(visible_latest),
                };

                if fetch_from < fetch_to {
                    // When the gap originates before the visible window,
                    // clamping `fetch_from` to `visible_earliest` can
                    // collapse the range into a sub-interval sliver (e.g.
                    // the first few seconds of a kline bucket that already
                    // has trades).  Skip fetches that cover less than one
                    // full interval.
                    if gap_start < visible_earliest {
                        let interval_ms = self.interval.to_milliseconds();
                        if fetch_to.as_u64().saturating_sub(fetch_from.as_u64()) < interval_ms {
                            return None;
                        }
                    }
                    Some((fetch_from, fetch_to))
                } else {
                    None
                }
            })
    }

    fn find_trade_gap(
        &self,
        visible_earliest: UnixMs,
        visible_latest: UnixMs,
    ) -> Option<(Option<UnixMs>, Option<UnixMs>)> {
        let empty_kline_time = self
            .datapoints
            .range(visible_earliest.floor_to(self.interval)..=visible_latest)
            .rev()
            .find(|(_, dp)| dp.footprint.trades.is_empty())
            .map(|(&time, _)| time);

        if let Some(target_time) = empty_kline_time {
            let last_t_before_gap = self
                .datapoints
                .range(..target_time)
                .rev()
                .find_map(|(_, dp)| dp.last_trade_time());

            let first_t_after_gap = self
                .datapoints
                .range(target_time.saturating_add(1)..)
                .find_map(|(_, dp)| dp.first_trade_time());

            Some((last_t_before_gap, first_t_after_gap))
        } else {
            None
        }
    }

    pub fn max_qty_ts_range(
        &self,
        cluster_kind: ClusterKind,
        earliest: UnixMs,
        latest: UnixMs,
        highest: Price,
        lowest: Price,
        group_step: PriceStep,
    ) -> Qty {
        let mut max_cluster_qty: Qty = Qty::default();

        self.datapoints
            .range(earliest..=latest)
            .for_each(|(_, dp)| {
                max_cluster_qty = max_cluster_qty.max(dp.footprint.max_cluster_qty_grouped(
                    cluster_kind,
                    highest,
                    lowest,
                    group_step,
                ));
            });

        max_cluster_qty
    }
}

impl TimeSeries<HeatmapDataPoint> {
    pub fn new(basis: Basis, tick_size: PriceStep) -> Self {
        let timeframe = match basis {
            Basis::Time(interval) => interval,
            Basis::Tick(_) => unimplemented!(),
        };

        Self {
            datapoints: BTreeMap::new(),
            interval: timeframe,
            tick_size,
        }
    }

    pub fn max_trade_qty_and_aggr_volume(&self, earliest: UnixMs, latest: UnixMs) -> (Qty, Qty) {
        let mut max_trade_qty = Qty::ZERO;
        let mut max_aggr_volume = Qty::ZERO;

        self.datapoints
            .range(earliest..=latest)
            .for_each(|(_, dp)| {
                let (mut buy_volume, mut sell_volume) = (Qty::ZERO, Qty::ZERO);

                dp.grouped_trades.iter().for_each(|trade| {
                    let trade_qty = trade.qty;
                    max_trade_qty = max_trade_qty.max(trade_qty);

                    if trade.is_sell {
                        sell_volume += trade_qty;
                    } else {
                        buy_volume += trade_qty;
                    }
                });

                max_aggr_volume = max_aggr_volume.max(buy_volume + sell_volume);
            });

        (max_trade_qty, max_aggr_volume)
    }

    pub fn max_trade_qty_in_range(
        &self,
        earliest: UnixMs,
        latest: UnixMs,
        highest: Price,
        lowest: Price,
    ) -> Qty {
        let mut max_trade_qty = Qty::default();

        self.datapoints
            .range(earliest..=latest)
            .for_each(|(_, dp)| {
                dp.grouped_trades.iter().for_each(|trade| {
                    if trade.price >= lowest && trade.price <= highest {
                        max_trade_qty = max_trade_qty.max(trade.qty);
                    }
                });
            });

        max_trade_qty
    }
}

impl From<&TimeSeries<KlineDataPoint>> for BTreeMap<UnixMs, exchange::Volume> {
    /// Converts datapoints into a map of timestamps and volume data
    fn from(timeseries: &TimeSeries<KlineDataPoint>) -> Self {
        timeseries
            .datapoints
            .iter()
            .map(|(time, dp)| (*time, dp.kline.volume))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_kline(time: u64) -> Kline {
        let price = Price::from_f64(100.0);
        Kline {
            time: UnixMs::new(time),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Volume::empty_buy_sell(),
        }
    }

    #[test]
    fn all_empty_footprints_request_visible_history_without_waiting_for_live_trade() {
        let interval = Timeframe::M5;
        let interval_ms = interval.to_milliseconds();
        let first = 1_800_000_000_000_u64;
        let second = first + interval_ms;
        let third = second + interval_ms;
        let series = TimeSeries::<KlineDataPoint>::new(
            interval,
            PriceStep {
                units: Price::from_f64(0.1).units,
            },
            &[empty_kline(first), empty_kline(second), empty_kline(third)],
        );

        assert_eq!(
            series.suggest_trade_fetch_range(UnixMs::new(second), UnixMs::new(third)),
            Some((UnixMs::new(second), UnixMs::new(third + interval_ms - 1)))
        );
    }

    #[test]
    fn offscreen_empty_bucket_does_not_hide_visible_trade_gap() {
        let interval = Timeframe::M5;
        let interval_ms = interval.to_milliseconds();
        let first = 1_800_000_000_000_u64;
        let second = first + interval_ms;
        let third = second + interval_ms;
        let offscreen = third + interval_ms;
        let mut series = TimeSeries::<KlineDataPoint>::new(
            interval,
            PriceStep {
                units: Price::from_f64(0.1).units,
            },
            &[
                empty_kline(first),
                empty_kline(second),
                empty_kline(third),
                empty_kline(offscreen),
            ],
        );
        let trades = [
            Trade {
                time: UnixMs::new(first + 1_000),
                price: Price::from_f64(100.0),
                qty: Qty::from_f64(1.0),
                is_sell: false,
            },
            Trade {
                time: UnixMs::new(third + 1_000),
                price: Price::from_f64(101.0),
                qty: Qty::from_f64(1.0),
                is_sell: true,
            },
        ];
        series.insert_trades_existing_buckets(&trades);

        let (fetch_from, fetch_to) = series
            .suggest_trade_fetch_range(UnixMs::new(first), UnixMs::new(third))
            .expect("the visible empty candle must produce a gap request");

        assert!(fetch_from < UnixMs::new(second + interval_ms));
        assert!(fetch_to >= UnixMs::new(second));
        assert!(fetch_to < UnixMs::new(offscreen));
    }
}
