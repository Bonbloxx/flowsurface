use crate::aggr;
use crate::chart::{
    kline::{ClusterKind, KlineTrades, NPoc, RenkoConfig},
    tpo::{Config as TpoConfig, Profile as TpoProfile},
};
use exchange::unit::Qty;
use exchange::unit::price::{Price, PriceStep};
use exchange::{Kline, Trade, Volume};

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct TickAccumulation {
    pub tick_count: usize,
    pub kline: Kline,
    pub footprint: KlineTrades,
    pub tpo: Option<TpoProfile>,
}

impl TickAccumulation {
    pub fn new(trade: &Trade, step: PriceStep) -> Self {
        let mut footprint = KlineTrades::new();
        footprint.add_trade_to_nearest_bin(trade, step);

        let kline = Kline {
            time: trade.time,
            open: trade.price,
            high: trade.price,
            low: trade.price,
            close: trade.price,
            volume: Volume::empty_buy_sell().add_trade_qty(trade.is_sell, trade.qty),
        };

        Self {
            tick_count: 1,
            kline,
            footprint,
            tpo: None,
        }
    }

    pub fn update_with_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.tick_count += 1;
        self.kline.high = self.kline.high.max(trade.price);
        self.kline.low = self.kline.low.min(trade.price);
        self.kline.close = trade.price;

        self.kline.volume = self.kline.volume.add_trade_qty(trade.is_sell, trade.qty);

        self.add_trade(trade, step);
    }

    pub(crate) fn empty_at(time: exchange::UnixMs, open: Price, current: Price) -> Self {
        Self {
            tick_count: 0,
            kline: Kline {
                time,
                open,
                high: open.max(current),
                low: open.min(current),
                close: current,
                volume: Volume::empty_buy_sell(),
            },
            footprint: KlineTrades::new(),
            tpo: None,
        }
    }

    fn add_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.footprint.add_trade_to_nearest_bin(trade, step);
    }

    pub fn max_cluster_qty(&self, cluster_kind: ClusterKind, highest: Price, lowest: Price) -> Qty {
        self.footprint
            .max_cluster_qty(cluster_kind, highest, lowest)
    }

    pub fn is_full(&self, interval: aggr::TickCount) -> bool {
        self.tick_count >= interval.0 as usize
    }

    pub fn poc_price(&self) -> Option<Price> {
        self.footprint.poc_price()
    }

    pub fn set_poc_status(&mut self, status: NPoc) {
        self.footprint.set_poc_status(status);
    }

    pub fn calculate_poc(&mut self) {
        self.footprint.calculate_poc();
    }

    pub fn volume_delta(&self) -> Qty {
        if self.kline.volume.is_directional() {
            self.kline.volume.delta()
        } else if !self.footprint.trades.is_empty() {
            self.footprint
                .trades
                .values()
                .fold(Qty::ZERO, |acc, group| acc + group.delta_qty())
        } else {
            Qty::ZERO
        }
    }

    /// Whether this tick accumulation has directional (buy vs sell) data.
    pub fn is_directional(&self) -> bool {
        !self.footprint.trades.is_empty() || self.kline.volume.is_directional()
    }
}

pub struct TickAggr {
    pub datapoints: Vec<TickAccumulation>,
    pub interval: aggr::TickCount,
    pub tick_size: PriceStep,
    renko: Option<RenkoState>,
    tpo: Option<TpoState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenkoDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
struct RenkoState {
    config: RenkoConfig,
    direction: Option<RenkoDirection>,
}

#[derive(Debug, Clone, Copy)]
struct TpoState {
    config: TpoConfig,
}

impl TickAggr {
    pub fn new(interval: aggr::TickCount, tick_size: PriceStep, raw_trades: &[Trade]) -> Self {
        let mut tick_aggr = Self {
            datapoints: Vec::new(),
            interval,
            tick_size,
            renko: None,
            tpo: None,
        };

        if !raw_trades.is_empty() {
            tick_aggr.insert_trades(raw_trades);
        }

        tick_aggr
    }

    pub fn new_renko(config: RenkoConfig, tick_size: PriceStep, raw_trades: &[Trade]) -> Self {
        let mut tick_aggr = Self {
            datapoints: Vec::new(),
            // One x-axis unit represents one Renko brick.
            interval: aggr::TickCount(1),
            tick_size,
            renko: Some(RenkoState {
                config: config.normalized(),
                direction: None,
            }),
            tpo: None,
        };

        if !raw_trades.is_empty() {
            tick_aggr.insert_trades(raw_trades);
        }

        tick_aggr
    }

    pub fn is_renko(&self) -> bool {
        self.renko.is_some()
    }

    pub fn new_tpo(config: TpoConfig, tick_size: PriceStep, raw_trades: &[Trade]) -> Self {
        Self::new_tpo_seeded(config, tick_size, &[], raw_trades)
    }

    /// Build TPO profiles from OHLC bars (Sierra/Quantower path), then refine
    /// with any live/recent trades.
    pub fn new_tpo_seeded(
        config: TpoConfig,
        tick_size: PriceStep,
        klines: &[Kline],
        raw_trades: &[Trade],
    ) -> Self {
        let mut tick_aggr = Self {
            datapoints: Vec::new(),
            // One x-axis unit represents one complete or developing profile.
            interval: aggr::TickCount(1),
            tick_size,
            renko: None,
            tpo: Some(TpoState {
                config: config.normalized(),
            }),
        };

        if !klines.is_empty() {
            tick_aggr.insert_tpo_klines(klines);
        }
        if !raw_trades.is_empty() {
            tick_aggr.insert_trades(raw_trades);
        }

        tick_aggr
    }

    pub fn is_tpo(&self) -> bool {
        self.tpo.is_some()
    }

    /// Seed/update TPO letters from time bars: each bar expands its letter
    /// bracket high–low (time-at-price). Volume is ignored.
    pub fn insert_tpo_klines(&mut self, klines: &[Kline]) {
        if klines.is_empty() || self.tpo.is_none() {
            return;
        }

        let Some(state) = self.tpo else {
            return;
        };
        let config = state.config.normalized();
        let row_step = config.row_step(self.tick_size);
        let mut dirty = std::collections::BTreeSet::new();

        let mut ordered = klines.to_vec();
        ordered.sort_by_key(|k| k.time);

        for kline in &ordered {
            if kline.high < kline.low {
                continue;
            }
            let profile_start = config.profile_start(kline.time);
            let index = match self
                .datapoints
                .binary_search_by_key(&profile_start, |dp| dp.kline.time)
            {
                Ok(index) => index,
                Err(index) => {
                    let mut accumulation =
                        TickAccumulation::empty_at(kline.time, kline.open, kline.close);
                    accumulation.kline.time = profile_start;
                    accumulation.kline.high = kline.high;
                    accumulation.kline.low = kline.low;
                    accumulation.tpo =
                        Some(TpoProfile::empty_at(config, profile_start, kline.open));
                    self.datapoints.insert(index, accumulation);
                    index
                }
            };

            let accumulation = &mut self.datapoints[index];
            accumulation.kline.high = accumulation.kline.high.max(kline.high);
            accumulation.kline.low = accumulation.kline.low.min(kline.low);
            if kline.time >= accumulation.kline.time || accumulation.tick_count == 0 {
                // Keep envelope OHLC; profile owns session open/close semantics.
            }
            if let Some(profile) = accumulation.tpo.as_mut() {
                profile.apply_kline(config, row_step, kline);
            }
            accumulation.tick_count = accumulation.tick_count.saturating_add(1);
            dirty.insert(profile_start);
        }

        for accumulation in &mut self.datapoints {
            if !dirty.contains(&accumulation.kline.time) {
                continue;
            }
            if let Some(profile) = accumulation.tpo.as_mut() {
                profile.recalculate(config, row_step);
            }
            Self::sync_tpo_kline_fields(accumulation);
        }
    }

    pub fn change_tick_size(&mut self, tick_size: PriceStep, raw_trades: &[Trade]) {
        self.tick_size = tick_size;

        self.datapoints.clear();
        if let Some(state) = &mut self.renko {
            state.direction = None;
        }

        if !raw_trades.is_empty() {
            self.insert_trades(raw_trades);
        }
    }

    /// return latest data point and its index
    pub fn latest_dp(&self) -> Option<(&TickAccumulation, usize)> {
        self.datapoints
            .last()
            .map(|dp| (dp, self.datapoints.len() - 1))
    }

    pub fn volume_data(&self) -> BTreeMap<u64, exchange::Volume> {
        self.into()
    }

    pub fn insert_trades(&mut self, buffer: &[Trade]) {
        if self.tpo.is_some() {
            self.insert_tpo_trades(buffer);
            return;
        }

        if self.renko.is_some() {
            self.insert_renko_trades(buffer);
            return;
        }

        let mut updated_indices = Vec::new();

        for trade in buffer {
            if self.datapoints.is_empty() {
                self.datapoints
                    .push(TickAccumulation::new(trade, self.tick_size));
                updated_indices.push(0);
            } else {
                let last_idx = self.datapoints.len() - 1;

                if self.datapoints[last_idx].is_full(self.interval) {
                    self.datapoints
                        .push(TickAccumulation::new(trade, self.tick_size));
                    updated_indices.push(self.datapoints.len() - 1);
                } else {
                    self.datapoints[last_idx].update_with_trade(trade, self.tick_size);
                    if !updated_indices.contains(&last_idx) {
                        updated_indices.push(last_idx);
                    }
                }
            }
        }

        for idx in updated_indices {
            if idx < self.datapoints.len() {
                self.datapoints[idx].calculate_poc();
            }
        }

        self.update_poc_status();
    }

    fn insert_renko_trades(&mut self, buffer: &[Trade]) {
        for trade in buffer {
            self.insert_renko_trade(trade);
        }

        self.update_poc_status();
    }

    fn insert_renko_trade(&mut self, trade: &Trade) {
        let Some(mut state) = self.renko else {
            return;
        };

        if self.datapoints.is_empty() {
            self.datapoints
                .push(TickAccumulation::new(trade, self.tick_size));
            return;
        }

        self.datapoints
            .last_mut()
            .expect("Renko datapoint initialized")
            .update_with_trade(trade, self.tick_size);

        let brick_units = self
            .tick_size
            .units
            .saturating_mul(i64::from(state.config.brick_size));
        if brick_units <= 0 {
            return;
        }

        loop {
            let current = self.datapoints.last().expect("Renko datapoint initialized");
            if trade.time.saturating_diff(current.kline.time)
                < u64::from(state.config.normalization_ms)
            {
                break;
            }

            let anchor = current.kline.open;
            let reversal_units = brick_units.saturating_mul(i64::from(state.config.reversal));
            let up_close = Price::from_units(anchor.units.saturating_add(brick_units));
            let down_close = Price::from_units(anchor.units.saturating_sub(brick_units));

            let completion = match state.direction {
                None if trade.price >= up_close => Some((RenkoDirection::Up, anchor, up_close)),
                None if trade.price <= down_close => {
                    Some((RenkoDirection::Down, anchor, down_close))
                }
                Some(RenkoDirection::Up) if trade.price >= up_close => {
                    Some((RenkoDirection::Up, anchor, up_close))
                }
                Some(RenkoDirection::Down) if trade.price <= down_close => {
                    Some((RenkoDirection::Down, anchor, down_close))
                }
                Some(RenkoDirection::Up)
                    if trade.price.units <= anchor.units.saturating_sub(reversal_units) =>
                {
                    let close = Price::from_units(anchor.units.saturating_sub(reversal_units));
                    let open = Price::from_units(close.units.saturating_add(brick_units));
                    Some((RenkoDirection::Down, open, close))
                }
                Some(RenkoDirection::Down)
                    if trade.price.units >= anchor.units.saturating_add(reversal_units) =>
                {
                    let close = Price::from_units(anchor.units.saturating_add(reversal_units));
                    let open = Price::from_units(close.units.saturating_sub(brick_units));
                    Some((RenkoDirection::Up, open, close))
                }
                _ => None,
            };

            let Some((direction, open, close)) = completion else {
                break;
            };

            let completed = self
                .datapoints
                .last_mut()
                .expect("Renko datapoint initialized");
            let is_synthetic = completed.tick_count == 0;
            completed.kline.open = open;
            completed.kline.close = close;
            if is_synthetic {
                completed.kline.high = open.max(close);
                completed.kline.low = open.min(close);
            } else {
                completed.kline.high = completed.kline.high.max(open).max(close);
                completed.kline.low = completed.kline.low.min(open).min(close);
            }
            completed.calculate_poc();

            state.direction = Some(direction);
            self.datapoints
                .push(TickAccumulation::empty_at(trade.time, close, trade.price));
        }

        self.renko = Some(state);
    }

    fn insert_tpo_trades(&mut self, buffer: &[Trade]) {
        if buffer.is_empty() {
            return;
        }

        // Apply all trades first, then recalculate each profile once.
        // Recalculating POC/VA per trade is prohibitively expensive on multi-day
        // BTC history (millions of trades). Track dirty profiles by session start
        // so inserts that shift datapoint indices stay correct.
        let mut dirty_starts = std::collections::BTreeSet::new();

        for trade in buffer {
            if let Some(start) = self.insert_tpo_trade_fast(trade) {
                dirty_starts.insert(start);
            }
        }

        let Some(state) = self.tpo else {
            return;
        };
        let config = state.config.normalized();
        let row_step = config.row_step(self.tick_size);

        for accumulation in &mut self.datapoints {
            if !dirty_starts.contains(&accumulation.kline.time) {
                continue;
            }
            if let Some(profile) = accumulation.tpo.as_mut() {
                profile.recalculate(config, row_step);
            }
            Self::sync_tpo_kline_fields(accumulation);
        }
    }

    /// Keep the candle envelope aligned with profile open/close and row extremes.
    fn sync_tpo_kline_fields(accumulation: &mut TickAccumulation) {
        let Some(profile) = accumulation.tpo.as_ref() else {
            return;
        };
        accumulation.kline.open = profile.open;
        accumulation.kline.close = profile.close;
        if let Some((&low, _)) = profile.rows.first_key_value() {
            accumulation.kline.low = low;
        }
        if let Some((&high, _)) = profile.rows.last_key_value() {
            accumulation.kline.high = high;
        }
    }

    /// Insert one trade without recalculating profile levels.
    /// Returns the profile session start that needs a later recalculate.
    fn insert_tpo_trade_fast(&mut self, trade: &Trade) -> Option<exchange::UnixMs> {
        let state = self.tpo?;
        let config = state.config.normalized();
        let profile_start = config.profile_start(trade.time);
        let row_step = config.row_step(self.tick_size);

        match self
            .datapoints
            .binary_search_by_key(&profile_start, |dp| dp.kline.time)
        {
            Ok(index) => {
                let accumulation = &mut self.datapoints[index];
                accumulation.update_with_trade(trade, row_step);
                if let Some(profile) = accumulation.tpo.as_mut() {
                    profile.apply_trade(config, row_step, trade);
                }
                Some(profile_start)
            }
            Err(index) => {
                let mut accumulation = TickAccumulation::new(trade, row_step);
                accumulation.kline.time = profile_start;
                // Profile::new already applies the first trade and recalculates;
                // that one-time cost per new session is fine.
                accumulation.tpo = Some(TpoProfile::new(config, row_step, trade));
                self.datapoints.insert(index, accumulation);
                Some(profile_start)
            }
        }
    }

    pub fn update_poc_status(&mut self) {
        let updates = self
            .datapoints
            .iter()
            .enumerate()
            .filter_map(|(idx, dp)| dp.poc_price().map(|price| (idx, price)))
            .collect::<Vec<_>>();

        let total_points = self.datapoints.len();

        for (current_idx, poc_price) in updates {
            let mut npoc = NPoc::default();

            for next_idx in (current_idx + 1)..total_points {
                let next_dp = &self.datapoints[next_idx];

                let next_dp_low = next_dp.kline.low.round_to_side_step(true, self.tick_size);
                let next_dp_high = next_dp.kline.high.round_to_side_step(false, self.tick_size);

                if next_dp_low <= poc_price && next_dp_high >= poc_price {
                    // on render we reverse the order of the points
                    // as it is easier to just take the idx=0 as latest candle for coords
                    let reversed_idx = (total_points - 1) - next_idx;
                    npoc.filled(reversed_idx as u64);
                    break;
                } else {
                    npoc.unfilled();
                }
            }

            if current_idx < total_points {
                let data_point = &mut self.datapoints[current_idx];
                data_point.set_poc_status(npoc);
            }
        }
    }

    pub fn min_max_price_in_range_prices(
        &self,
        earliest: usize,
        latest: usize,
    ) -> Option<(Price, Price)> {
        if earliest > latest {
            return None;
        }

        let mut min_p: Option<Price> = None;
        let mut max_p: Option<Price> = None;

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(idx, _)| *idx >= earliest && *idx <= latest)
            .for_each(|(_, dp)| {
                let low = dp.kline.low;
                let high = dp.kline.high;

                min_p = Some(match min_p {
                    Some(value) => value.min(low),
                    None => low,
                });
                max_p = Some(match max_p {
                    Some(value) => value.max(high),
                    None => high,
                });
            });

        match (min_p, max_p) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        }
    }

    pub fn min_max_price_in_range(&self, earliest: usize, latest: usize) -> Option<(f32, f32)> {
        self.min_max_price_in_range_prices(earliest, latest)
            .map(|(min_p, max_p)| (min_p.to_f32_lossy(), max_p.to_f32_lossy()))
    }

    pub fn min_max_footprint_price_in_range(
        &self,
        earliest: usize,
        latest: usize,
    ) -> Option<(Price, Price)> {
        if earliest > latest {
            return None;
        }

        let mut min_p: Option<Price> = None;
        let mut max_p: Option<Price> = None;

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(idx, _)| *idx >= earliest && *idx <= latest)
            .for_each(|(_, dp)| {
                min_p = Some(match min_p {
                    Some(value) => value.min(dp.kline.low),
                    None => dp.kline.low,
                });
                max_p = Some(match max_p {
                    Some(value) => value.max(dp.kline.high),
                    None => dp.kline.high,
                });

                for price in dp.footprint.trades.keys() {
                    min_p = Some(match min_p {
                        Some(value) => value.min(*price),
                        None => *price,
                    });
                    max_p = Some(match max_p {
                        Some(value) => value.max(*price),
                        None => *price,
                    });
                }
            });

        match (min_p, max_p) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        }
    }

    pub fn min_max_tpo_price_in_range(
        &self,
        earliest: usize,
        latest: usize,
    ) -> Option<(Price, Price)> {
        if earliest > latest {
            return None;
        }

        let mut min_p: Option<Price> = None;
        let mut max_p: Option<Price> = None;

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(idx, _)| *idx >= earliest && *idx <= latest)
            .filter_map(|(_, dp)| dp.tpo.as_ref())
            .for_each(|profile| {
                if let Some((price, _)) = profile.rows.first_key_value() {
                    min_p = Some(min_p.map_or(*price, |value| value.min(*price)));
                }
                if let Some((price, _)) = profile.rows.last_key_value() {
                    max_p = Some(max_p.map_or(*price, |value| value.max(*price)));
                }
            });

        min_p.zip(max_p)
    }

    pub fn max_qty_idx_range(
        &self,
        cluster_kind: ClusterKind,
        earliest: usize,
        latest: usize,
        highest: Price,
        lowest: Price,
    ) -> Qty {
        let mut max_cluster_qty: Qty = Qty::default();

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(index, _)| *index <= latest && *index >= earliest)
            .for_each(|(_, dp)| {
                max_cluster_qty =
                    max_cluster_qty.max(dp.max_cluster_qty(cluster_kind, highest, lowest));
            });

        max_cluster_qty
    }
}

impl From<&TickAggr> for BTreeMap<u64, exchange::Volume> {
    /// Converts datapoints into a map of timestamps and volume data
    fn from(tick_aggr: &TickAggr) -> Self {
        tick_aggr
            .datapoints
            .iter()
            .enumerate()
            .map(|(idx, dp)| (idx as u64, dp.kline.volume))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::tpo::{BlockSize, Config as TpoConfig, ProfilePeriod};

    fn trade(time: u64, price: f64) -> Trade {
        Trade {
            time: exchange::UnixMs::new(time),
            is_sell: false,
            price: Price::from_f64(price),
            qty: Qty::from_f64(1.0),
        }
    }

    fn one_dollar_step() -> PriceStep {
        PriceStep {
            units: Price::from_f64(1.0).units,
        }
    }

    #[test]
    fn renko_builds_fixed_continuation_bricks_and_two_box_reversals() {
        let config = RenkoConfig {
            brick_size: 10,
            reversal: 2,
            normalization_ms: 0,
        };
        let mut aggr = TickAggr::new_renko(config, one_dollar_step(), &[]);

        aggr.insert_trades(&[
            trade(0, 100.0),
            trade(1, 111.0),
            trade(2, 121.0),
            trade(3, 99.0),
        ]);

        assert_eq!(aggr.datapoints.len(), 4);
        assert_eq!(aggr.datapoints[0].kline.open, Price::from_f64(100.0));
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f64(110.0));
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f64(110.0));
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(120.0));

        // A two-box reversal confirms at 100, but its fixed-size body begins at 110.
        assert_eq!(aggr.datapoints[2].kline.open, Price::from_f64(110.0));
        assert_eq!(aggr.datapoints[2].kline.close, Price::from_f64(100.0));
        assert_eq!(aggr.datapoints[3].kline.open, Price::from_f64(100.0));
        assert_eq!(aggr.datapoints[3].kline.close, Price::from_f64(99.0));
    }

    #[test]
    fn renko_brick_size_setting_rebuilds_to_a_different_true_profile() {
        let trades = [trade(0, 100.0), trade(1, 135.0)];
        let small = TickAggr::new_renko(
            RenkoConfig {
                brick_size: 10,
                reversal: 2,
                normalization_ms: 0,
            },
            one_dollar_step(),
            &trades,
        );
        let large = TickAggr::new_renko(
            RenkoConfig {
                brick_size: 20,
                reversal: 2,
                normalization_ms: 0,
            },
            one_dollar_step(),
            &trades,
        );

        assert_eq!(small.datapoints.len(), 4);
        assert_eq!(large.datapoints.len(), 2);
        assert_eq!(small.datapoints[0].kline.close, Price::from_f64(110.0));
        assert_eq!(large.datapoints[0].kline.close, Price::from_f64(120.0));
    }

    #[test]
    fn tpo_marks_every_traversed_row_and_calculates_standard_levels() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            initial_balance_blocks: 1,
            ..TpoConfig::default()
        };
        let trades = [
            trade(0, 100.0),
            trade(10 * 60_000, 102.0),
            trade(30 * 60_000, 101.0),
            trade(40 * 60_000, 103.0),
        ];
        let aggr = TickAggr::new_tpo(config, one_dollar_step(), &trades);

        assert_eq!(aggr.datapoints.len(), 1);
        let profile = aggr.datapoints[0].tpo.as_ref().expect("TPO profile");
        assert_eq!(profile.rows[&Price::from_f64(100.0)].blocks, vec![0]);
        assert_eq!(profile.rows[&Price::from_f64(101.0)].blocks, vec![0, 1]);
        assert_eq!(profile.rows[&Price::from_f64(102.0)].blocks, vec![0, 1]);
        assert_eq!(profile.rows[&Price::from_f64(103.0)].blocks, vec![1]);
        assert_eq!(profile.poc, Price::from_f64(101.0));
        assert_eq!(profile.initial_balance_low, Price::from_f64(100.0));
        assert_eq!(profile.initial_balance_high, Price::from_f64(102.0));
    }

    #[test]
    fn tpo_settings_change_rows_blocks_sessions_and_initial_balance() {
        let trades = [
            trade(0, 100.0),
            trade(30 * 60_000, 104.0),
            trade(60 * 60_000, 108.0),
        ];
        let fine = TickAggr::new_tpo(
            TpoConfig {
                profile_period: ProfilePeriod::Hours2,
                block_size: BlockSize::Minutes30,
                ticks_per_row: 1,
                initial_balance_blocks: 1,
                ..TpoConfig::default()
            },
            one_dollar_step(),
            &trades,
        );
        let coarse = TickAggr::new_tpo(
            TpoConfig {
                profile_period: ProfilePeriod::Hour,
                block_size: BlockSize::Hour,
                ticks_per_row: 2,
                initial_balance_blocks: 1,
                ..TpoConfig::default()
            },
            one_dollar_step(),
            &trades,
        );

        let fine_profile = fine.datapoints[0].tpo.as_ref().expect("fine TPO");
        let coarse_profile = coarse.datapoints[0].tpo.as_ref().expect("coarse TPO");
        assert_eq!(fine.datapoints.len(), 1);
        assert_eq!(coarse.datapoints.len(), 2);
        assert_ne!(
            fine_profile.rows.keys().collect::<Vec<_>>(),
            coarse_profile.rows.keys().collect::<Vec<_>>()
        );
        assert_eq!(fine_profile.initial_balance_high, Price::from_f64(100.0));
        assert_eq!(coarse_profile.initial_balance_high, Price::from_f64(104.0));
    }

    #[test]
    fn tpo_batch_order_does_not_change_open_close_or_profile_rows() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            ..TpoConfig::default()
        };
        let ordered = [trade(1, 100.0), trade(2, 102.0), trade(3, 101.0)];
        let reversed = [ordered[2], ordered[1], ordered[0]];
        let a = TickAggr::new_tpo(config, one_dollar_step(), &ordered);
        let b = TickAggr::new_tpo(config, one_dollar_step(), &reversed);
        let a_profile = a.datapoints[0].tpo.as_ref().expect("ordered TPO");
        let b_profile = b.datapoints[0].tpo.as_ref().expect("reversed TPO");

        assert_eq!(a_profile.open, b_profile.open);
        assert_eq!(a_profile.close, b_profile.close);
        assert_eq!(
            a_profile.rows.keys().collect::<Vec<_>>(),
            b_profile.rows.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tpo_splits_trades_into_session_aligned_profiles_with_letter_sequence() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            session_start_minutes_utc: 0,
            initial_balance_blocks: 1,
            value_area_percent: 70,
            ..TpoConfig::default()
        };
        // Hour 0: blocks A (0-30m) and B (30-60m)
        // Hour 1: block A of the next profile
        let trades = [
            trade(5 * 60_000, 100.0),
            trade(10 * 60_000, 102.0),
            trade(35 * 60_000, 101.0),
            trade(40 * 60_000, 103.0),
            trade(65 * 60_000, 102.0),
            trade(70 * 60_000, 104.0),
        ];
        let aggr = TickAggr::new_tpo(config, one_dollar_step(), &trades);

        assert_eq!(aggr.datapoints.len(), 2, "two hourly profiles");
        let day0 = aggr.datapoints[0].tpo.as_ref().expect("first profile");
        let day1 = aggr.datapoints[1].tpo.as_ref().expect("second profile");

        assert_eq!(day0.start, exchange::UnixMs::new(0));
        assert_eq!(day1.start, exchange::UnixMs::new(3_600_000));

        // First profile: A covers 100-102, B covers 101-103
        assert_eq!(day0.rows[&Price::from_f64(100.0)].blocks, vec![0]);
        assert_eq!(day0.rows[&Price::from_f64(101.0)].blocks, vec![0, 1]);
        assert_eq!(day0.rows[&Price::from_f64(102.0)].blocks, vec![0, 1]);
        assert_eq!(day0.rows[&Price::from_f64(103.0)].blocks, vec![1]);
        assert_eq!(day0.poc, Price::from_f64(101.0));
        assert_eq!(day0.initial_balance_low, Price::from_f64(100.0));
        assert_eq!(day0.initial_balance_high, Price::from_f64(102.0));

        // Second profile restarts letter sequence at A.
        assert_eq!(day1.rows[&Price::from_f64(102.0)].blocks, vec![0]);
        assert_eq!(day1.rows[&Price::from_f64(103.0)].blocks, vec![0]);
        assert_eq!(day1.rows[&Price::from_f64(104.0)].blocks, vec![0]);
    }

    #[test]
    fn tpo_bulk_insert_builds_multiple_letters_without_per_trade_recalc_blowup() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes5,
            ticks_per_row: 1,
            ..TpoConfig::default()
        };
        // 12 five-minute blocks across one hour, a few trades each.
        let mut trades = Vec::new();
        for block in 0..12u64 {
            let base = block * 5 * 60_000;
            for i in 0..50u64 {
                trades.push(trade(base + i, 100.0 + (block % 5) as f64 + (i % 3) as f64));
            }
        }
        let start = std::time::Instant::now();
        let aggr = TickAggr::new_tpo(config, one_dollar_step(), &trades);
        assert!(
            start.elapsed().as_millis() < 2_000,
            "bulk TPO insert should stay interactive"
        );
        assert_eq!(aggr.datapoints.len(), 1);
        let profile = aggr.datapoints[0].tpo.as_ref().expect("profile");
        // Multiple letters present on the value area.
        let max_count = profile.rows.values().map(|r| r.count()).max().unwrap_or(0);
        assert!(
            max_count >= 3,
            "expected multi-letter rows, max={max_count}"
        );
        assert!(profile.total_tpos > 20);
    }

    #[test]
    fn tpo_from_klines_marks_high_low_range_per_letter_like_sierra() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Day,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            session_start_minutes_utc: 0,
            profiles_to_load: 60,
            ..TpoConfig::default()
        };
        // Letter A: 100-102, Letter B: 101-104 → rows build multi-letter body.
        let klines = [
            Kline {
                time: exchange::UnixMs::new(0),
                open: Price::from_f64(100.0),
                high: Price::from_f64(102.0),
                low: Price::from_f64(100.0),
                close: Price::from_f64(101.0),
                volume: Volume::empty_buy_sell(),
            },
            Kline {
                time: exchange::UnixMs::new(30 * 60_000),
                open: Price::from_f64(101.0),
                high: Price::from_f64(104.0),
                low: Price::from_f64(101.0),
                close: Price::from_f64(103.0),
                volume: Volume::empty_buy_sell(),
            },
        ];
        let aggr = TickAggr::new_tpo_seeded(config, one_dollar_step(), &klines, &[]);
        assert_eq!(aggr.datapoints.len(), 1);
        let profile = aggr.datapoints[0].tpo.as_ref().expect("profile");
        assert_eq!(profile.rows[&Price::from_f64(100.0)].blocks, vec![0]);
        assert_eq!(profile.rows[&Price::from_f64(101.0)].blocks, vec![0, 1]);
        assert_eq!(profile.rows[&Price::from_f64(102.0)].blocks, vec![0, 1]);
        assert_eq!(profile.rows[&Price::from_f64(103.0)].blocks, vec![1]);
        assert_eq!(profile.rows[&Price::from_f64(104.0)].blocks, vec![1]);
        assert_eq!(profile.open, Price::from_f64(100.0));
        assert_eq!(profile.close, Price::from_f64(103.0));
    }

    #[test]
    fn tpo_klines_split_into_multiple_daily_profiles() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Day,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            session_start_minutes_utc: 0,
            profiles_to_load: 60,
            ..TpoConfig::default()
        };
        let day_ms = 86_400_000u64;
        let klines = [
            Kline {
                time: exchange::UnixMs::new(1_000),
                open: Price::from_f64(100.0),
                high: Price::from_f64(101.0),
                low: Price::from_f64(100.0),
                close: Price::from_f64(100.5),
                volume: Volume::empty_buy_sell(),
            },
            Kline {
                time: exchange::UnixMs::new(day_ms + 1_000),
                open: Price::from_f64(110.0),
                high: Price::from_f64(111.0),
                low: Price::from_f64(110.0),
                close: Price::from_f64(110.5),
                volume: Volume::empty_buy_sell(),
            },
        ];
        let aggr = TickAggr::new_tpo_seeded(config, one_dollar_step(), &klines, &[]);
        assert_eq!(aggr.datapoints.len(), 2, "two daily profiles");
    }

    #[test]
    fn tpo_off_grid_trade_does_not_double_count_adjacent_rows() {
        let config = TpoConfig {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 10, // $1 rows with $0.1 tick
            ..TpoConfig::default()
        };
        let step = PriceStep {
            units: Price::from_f64(0.1).units,
        };
        let aggr = TickAggr::new_tpo(config, step, &[trade(0, 100.5)]);
        let profile = aggr.datapoints[0].tpo.as_ref().expect("TPO profile");
        assert_eq!(profile.rows.len(), 1);
        assert!(profile.rows.contains_key(&Price::from_f64(101.0)));
    }

    #[test]
    fn renko_normalization_limits_flash_bricks_without_losing_price() {
        let config = RenkoConfig {
            brick_size: 10,
            reversal: 2,
            normalization_ms: 1_000,
        };
        let mut aggr = TickAggr::new_renko(config, one_dollar_step(), &[]);

        aggr.insert_trades(&[trade(0, 100.0), trade(500, 120.0)]);
        assert_eq!(aggr.datapoints.len(), 1);
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f64(120.0));

        aggr.insert_trades(&[trade(1_000, 120.0)]);
        assert_eq!(aggr.datapoints.len(), 2);
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f64(110.0));
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(120.0));

        aggr.insert_trades(&[trade(2_000, 120.0)]);
        assert_eq!(aggr.datapoints.len(), 3);
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(120.0));
    }

    #[test]
    fn renko_fills_multi_brick_jumps_without_repeating_synthetic_wicks() {
        let config = RenkoConfig {
            brick_size: 10,
            reversal: 2,
            normalization_ms: 0,
        };
        let mut aggr = TickAggr::new_renko(config, one_dollar_step(), &[]);

        aggr.insert_trades(&[trade(0, 100.0), trade(1, 135.0)]);

        assert_eq!(aggr.datapoints.len(), 4);
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f64(110.0));
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(120.0));
        assert_eq!(aggr.datapoints[1].kline.high, Price::from_f64(120.0));
        assert_eq!(aggr.datapoints[2].kline.open, Price::from_f64(120.0));
        assert_eq!(aggr.datapoints[2].kline.close, Price::from_f64(130.0));
        assert_eq!(aggr.datapoints[2].kline.high, Price::from_f64(130.0));
        assert_eq!(aggr.datapoints[3].kline.close, Price::from_f64(135.0));
    }

    #[test]
    fn renko_configuration_is_clamped_to_safe_limits() {
        let config = RenkoConfig {
            brick_size: 0,
            reversal: 9,
            normalization_ms: 50_000,
        }
        .normalized();

        assert_eq!(config.brick_size, RenkoConfig::MIN_BRICK_SIZE);
        assert_eq!(config.reversal, RenkoConfig::MAX_REVERSAL);
        assert_eq!(config.normalization_ms, 10_000);
    }
}
