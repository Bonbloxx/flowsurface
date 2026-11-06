use super::{RenkoState, TickAccumulation, TickAggr};
use crate::chart::kline::RenkoConfig;
use exchange::{
    Ticker, Trade, UnixMs, Volume,
    unit::{Price, PriceStep, Qty},
};
use serde::{Deserialize, Serialize};

/// Compact chart state, stored with its pane rather than in a second trade store.
/// The last bar is developing; its start time and the last processed prints are
/// needed to resume minimum-lifetime and equal-millisecond replay exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenkoSnapshot {
    version: u8,
    ticker: Ticker,
    step: PriceStep,
    state: RenkoState,
    bars: Vec<Bar>,
    last_trades: Vec<Print>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bar {
    time: UnixMs,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    buy: Qty,
    sell: Qty,
    count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Print {
    time: UnixMs,
    price: Price,
    qty: Qty,
    is_sell: bool,
}

impl TickAggr {
    pub const RENKO_HISTORY_WINDOW_MS: u64 = 4 * 60 * 60 * 1_000;
    pub const MAX_SAVED_RENKO_BRICKS: usize = 50_000;

    /// Keep recent confirmed bricks without rebuilding their price anchor or
    /// the developing brick. Expired checkpoints start afresh in this window.
    pub fn trim_renko_history(&mut self, now: UnixMs) -> bool {
        let Some(state) = self.renko.as_deref() else {
            return false;
        };
        let cutoff = now.saturating_sub(Self::RENKO_HISTORY_WINDOW_MS);
        if state.last_input_time.is_some_and(|last| last < cutoff) {
            *self = Self::new_renko(state.config, self.tick_size, &[]);
            self.renko_trimmed = true;
            return true;
        }
        // The final brick carries the continuation state even if a quiet
        // market has left it developing longer than the visible window.
        let confirmed = self.datapoints.len().saturating_sub(1);
        let expired = self.datapoints[..confirmed].partition_point(|dp| dp.kline.time < cutoff);
        let excess = expired.max(
            self.datapoints
                .len()
                .saturating_sub(Self::MAX_SAVED_RENKO_BRICKS + 1),
        );
        if excess > 0 {
            self.datapoints.drain(..excess);
        }
        self.renko_trimmed = excess > 0;
        self.renko_trimmed
    }

    pub fn renko_snapshot(&self, ticker: Ticker) -> Option<Box<RenkoSnapshot>> {
        let state = *self.renko.as_deref()?;
        state.last_input_time?;
        Some(Box::new(RenkoSnapshot {
            version: 1,
            ticker,
            step: self.tick_size,
            state,
            bars: self
                .datapoints
                .iter()
                .skip(
                    self.datapoints
                        .len()
                        .saturating_sub(Self::MAX_SAVED_RENKO_BRICKS + 1),
                )
                .map(|dp| Bar {
                    time: dp.kline.time,
                    open: dp.kline.open,
                    high: dp.kline.high,
                    low: dp.kline.low,
                    close: dp.kline.close,
                    buy: dp.kline.volume.buy_qty_or_zero(),
                    sell: dp.kline.volume.sell_qty_or_zero(),
                    count: dp.tick_count,
                })
                .collect(),
            last_trades: self
                .renko_last_trades
                .iter()
                .map(|t| Print {
                    time: t.time,
                    price: t.price,
                    qty: t.qty,
                    is_sell: t.is_sell,
                })
                .collect(),
        }))
    }

    pub fn restore_renko(
        snapshot: &RenkoSnapshot,
        ticker: Ticker,
        config: RenkoConfig,
        step: PriceStep,
    ) -> Option<Self> {
        let last_time = snapshot.state.last_input_time?;
        let brick = step.units.checked_mul(i64::from(config.brick_size))?;
        if snapshot.version != 1
            || !snapshot.ticker.same_market(&ticker)
            || snapshot.step != step
            || snapshot.state.config != config.normalized()
            || brick <= 0
            || snapshot.bars.is_empty()
            || snapshot.bars.len() > Self::MAX_SAVED_RENKO_BRICKS + 1
            || !snapshot
                .bars
                .windows(2)
                .all(|pair| pair[0].time <= pair[1].time)
            || snapshot.bars.iter().enumerate().any(|(i, bar)| {
                bar.time > last_time
                    || bar.low > bar.open.min(bar.close)
                    || bar.high < bar.open.max(bar.close)
                    || bar.low > bar.high
                    || bar.buy.units < 0
                    || bar.sell.units < 0
                    || (i + 1 < snapshot.bars.len()
                        && bar.close.units.abs_diff(bar.open.units) != brick as u64)
            })
            || snapshot
                .last_trades
                .iter()
                .any(|t| t.time != last_time || t.qty.units < 0)
        {
            return None;
        }
        let mut aggr = Self::new_renko(config, step, &[]);
        aggr.renko = Some(Box::new(snapshot.state));
        aggr.datapoints = snapshot
            .bars
            .iter()
            .map(|bar| {
                let mut dp = TickAccumulation::empty_at(bar.time, bar.open, bar.close);
                dp.kline.high = bar.high;
                dp.kline.low = bar.low;
                dp.kline.volume = Volume::BuySell(bar.buy, bar.sell);
                dp.tick_count = bar.count;
                dp
            })
            .collect();
        aggr.renko_last_trades = snapshot
            .last_trades
            .iter()
            .map(|t| Trade {
                time: t.time,
                price: t.price,
                qty: t.qty,
                is_sell: t.is_sell,
            })
            .collect();
        Some(aggr)
    }

    pub fn renko_last_input_time(&self) -> Option<UnixMs> {
        self.renko.as_ref().and_then(|state| state.last_input_time)
    }

    pub fn renko_boundary_trades(&self) -> &[Trade] {
        &self.renko_last_trades
    }

    pub fn renko_trimmed_on_last_insert(&self) -> bool {
        self.renko_trimmed
    }

    /// Match the drawn Renko range, which excludes the developing projection.
    /// Hidden wicks must not squeeze the visible brick bodies into empty space.
    pub fn renko_visible_price_range(
        &self,
        earliest: usize,
        latest: usize,
        show_wicks: bool,
    ) -> Option<(Price, Price)> {
        self.renko.as_ref()?;
        let base = self.datapoints.len().checked_sub(2)?;
        let upper = base.checked_sub(earliest)?;
        let lower = base.saturating_sub(latest);
        if lower > upper {
            return None;
        }
        self.datapoints[lower..=upper]
            .iter()
            .map(|dp| {
                if show_wicks {
                    (dp.kline.low, dp.kline.high)
                } else {
                    (
                        dp.kline.open.min(dp.kline.close),
                        dp.kline.open.max(dp.kline.close),
                    )
                }
            })
            .reduce(|(low, high), (next_low, next_high)| (low.min(next_low), high.max(next_high)))
    }

    /// Preserve confirmed history across a gap whose executions are unavailable.
    /// Start a new segment instead of inventing bricks between its endpoints.
    pub fn resume_renko_after_gap(&mut self, trades: &[Trade]) {
        let Some(state) = self.renko.as_deref() else {
            return;
        };
        if trades.is_empty() {
            return;
        }
        let mut resumed = Self::new_renko(state.config, self.tick_size, trades);
        self.datapoints.pop();
        self.datapoints.append(&mut resumed.datapoints);
        self.renko = resumed.renko;
        self.renko_last_trades = resumed.renko_last_trades;
        if let Some(last) = self.renko_last_input_time() {
            self.trim_renko_history(last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;

    fn ticker() -> Ticker {
        Ticker::new("BTCUSDT", Exchange::BinanceLinear)
    }
    fn step() -> PriceStep {
        PriceStep {
            units: Price::from_f64(0.1).units,
        }
    }
    fn config() -> RenkoConfig {
        RenkoConfig {
            brick_size: 200,
            reversal: 2,
            normalization_ms: 250,
        }
    }
    fn trade(time: u64, price: f64) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(1.0),
            is_sell: false,
        }
    }
    fn snapshot(aggr: &TickAggr) -> String {
        serde_json::to_string(&aggr.renko_snapshot(ticker()).unwrap()).unwrap()
    }

    #[test]
    fn restart_at_every_trade_preserves_bricks_wicks_volume_and_lifetime() {
        let trades = [
            trade(0, 100.),
            trade(100, 135.),
            trade(250, 125.),
            trade(250, 145.),
            trade(499, 160.),
            trade(500, 160.),
            trade(750, 160.),
            trade(1000, 119.),
            trade(1250, 90.),
        ];
        let expected = snapshot(&TickAggr::new_renko(config(), step(), &trades));
        for split in 1..trades.len() {
            let before = TickAggr::new_renko(config(), step(), &trades[..split]);
            let saved: RenkoSnapshot = serde_json::from_str(&snapshot(&before)).unwrap();
            let mut resumed = TickAggr::restore_renko(&saved, ticker(), config(), step()).unwrap();
            assert_eq!(snapshot(&resumed), snapshot(&before));
            resumed.insert_trades(&trades[split..]);
            assert_eq!(snapshot(&resumed), expected, "restart at split {split}");
        }
    }

    #[test]
    fn historical_trade_replay_uses_the_same_lifetime_as_live_batches() {
        let trades = [
            trade(0, 100.),
            trade(60_000, 120.),
            trade(60_250, 140.),
            trade(60_500, 160.),
        ];
        let replay = TickAggr::new_renko(config(), step(), &trades);
        let mut live = TickAggr::new_renko(config(), step(), &[]);
        for t in trades {
            live.insert_trades(&[t]);
        }
        assert_eq!(snapshot(&replay), snapshot(&live));
        assert_eq!(
            replay.datapoints.len(),
            4,
            "all three 20-dollar bricks survive replay"
        );
    }

    #[test]
    fn rolling_four_hour_history_preserves_recent_bricks_and_restart_state() {
        let hour = 60 * 60 * 1_000;
        let trades = (0..=6)
            .map(|i| trade(i * hour, 100. + i as f64 * 20.))
            .collect::<Vec<_>>();
        let expected = TickAggr::new_renko(config(), step(), &trades);
        assert_eq!(expected.datapoints.len(), 5);
        assert!(
            expected
                .datapoints
                .iter()
                .all(|dp| dp.kline.time >= UnixMs::new(2 * hour))
        );

        let before = TickAggr::new_renko(config(), step(), &trades[..6]);
        let saved = before.renko_snapshot(ticker()).unwrap();
        let mut restored = TickAggr::restore_renko(&saved, ticker(), config(), step()).unwrap();
        restored.trim_renko_history(UnixMs::new(6 * hour));
        restored.insert_trades(&trades[6..]);
        assert_eq!(snapshot(&restored), snapshot(&expected));
        assert!(
            restored
                .renko_snapshot(ticker())
                .unwrap()
                .bars
                .iter()
                .all(|bar| bar.time >= UnixMs::new(2 * hour))
        );
    }

    #[test]
    fn trimming_keeps_a_quiet_developing_brick_until_its_checkpoint_expires() {
        let mut aggr = TickAggr::new_renko(config(), step(), &[trade(0, 100.)]);
        let end = TickAggr::RENKO_HISTORY_WINDOW_MS + 1;
        aggr.insert_trades(&[trade(end, 101.)]);
        assert_eq!(aggr.datapoints.len(), 1);
        assert_eq!(aggr.datapoints[0].kline.open, Price::from_f64(100.));
        assert!(!aggr.trim_renko_history(UnixMs::new(end)));

        assert!(aggr.trim_renko_history(UnixMs::new(end + TickAggr::RENKO_HISTORY_WINDOW_MS + 1)));
        assert!(aggr.datapoints.is_empty());
        assert!(aggr.renko_snapshot(ticker()).is_none());
        aggr.insert_trades(&[trade(end * 2 + 1, 1000.)]);
        assert_eq!(
            aggr.datapoints.len(),
            1,
            "an expired anchor must not manufacture bricks"
        );
        assert_eq!(aggr.datapoints[0].kline.open, Price::from_f64(1000.));
    }

    #[test]
    fn snapshot_rejects_wrong_market_configuration_and_invalid_bodies() {
        let aggr = TickAggr::new_renko(config(), step(), &[trade(0, 100.), trade(250, 120.)]);
        let mut saved = aggr.renko_snapshot(ticker()).unwrap();
        assert!(
            TickAggr::restore_renko(
                &saved,
                Ticker::new("ETHUSDT", Exchange::BinanceLinear),
                config(),
                step()
            )
            .is_none()
        );
        assert!(
            TickAggr::restore_renko(
                &saved,
                ticker(),
                RenkoConfig {
                    normalization_ms: 500,
                    ..config()
                },
                step()
            )
            .is_none()
        );
        assert!(
            TickAggr::restore_renko(
                &saved,
                ticker(),
                config(),
                PriceStep {
                    units: step().units * 2
                }
            )
            .is_none()
        );
        saved.bars[0].close = Price::from_f64(119.);
        assert!(TickAggr::restore_renko(&saved, ticker(), config(), step()).is_none());
    }

    #[test]
    fn missing_history_does_not_invent_bricks_across_an_outage() {
        let mut aggr = TickAggr::new_renko(config(), step(), &[trade(0, 100.), trade(250, 120.)]);
        aggr.resume_renko_after_gap(&[trade(100_000, 1000.), trade(100_250, 1020.)]);
        assert_eq!(aggr.datapoints.len(), 3);
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f64(120.));
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f64(1000.));
        let saved = aggr.renko_snapshot(ticker()).unwrap();
        assert!(TickAggr::restore_renko(&saved, ticker(), config(), step()).is_some());
    }

    #[test]
    fn renko_scaling_matches_drawn_bodies_and_ignores_the_projection() {
        let mut aggr = TickAggr::new_renko(
            config(),
            step(),
            &[
                trade(0, 100.),
                trade(100, 999.),
                trade(250, 120.),
                trade(251, 5000.),
            ],
        );
        assert_eq!(
            aggr.renko_visible_price_range(0, 10, false),
            Some((Price::from_f64(100.), Price::from_f64(120.)))
        );
        assert_eq!(
            aggr.renko_visible_price_range(0, 10, true),
            Some((Price::from_f64(100.), Price::from_f64(999.)))
        );
        aggr.insert_trades(&[trade(500, 140.)]);
        assert_eq!(
            aggr.renko_visible_price_range(0, 0, false),
            Some((Price::from_f64(120.), Price::from_f64(140.)))
        );
        assert_eq!(
            aggr.renko_visible_price_range(1, 1, false),
            Some((Price::from_f64(100.), Price::from_f64(120.)))
        );
        assert_eq!(
            TickAggr::new_renko(config(), step(), &[trade(0, 100.)])
                .renko_visible_price_range(0, 10, false),
            None
        );
    }

    #[test]
    fn renko_pane_roundtrip_and_legacy_layout_loading() {
        let aggr = TickAggr::new_renko(config(), step(), &[trade(0, 100.), trade(250, 120.)]);
        let pane = crate::Pane::KlineChart {
            layout: Default::default(),
            kind: crate::chart::KlineChartKind::Renko { config: config() },
            renko_state: aggr.renko_snapshot(ticker()),
            trev_state: None,
            orderflow_state: None,
            stream_type: vec![],
            settings: Default::default(),
            indicators: vec![],
            link_group: None,
        };
        let mut value = serde_json::to_value(&pane).unwrap();
        let loaded: crate::Pane = serde_json::from_value(value.clone()).unwrap();
        let crate::Pane::KlineChart {
            renko_state: Some(saved),
            ..
        } = loaded
        else {
            panic!()
        };
        let restored = TickAggr::restore_renko(&saved, ticker(), config(), step()).unwrap();
        assert_eq!(snapshot(&restored), snapshot(&aggr));
        value["KlineChart"]
            .as_object_mut()
            .unwrap()
            .remove("renko_state");
        assert!(matches!(
            serde_json::from_value::<crate::Pane>(value.clone()).unwrap(),
            crate::Pane::KlineChart {
                renko_state: None,
                ..
            }
        ));
        value["KlineChart"]["renko_state"] = serde_json::json!({"version": 999});
        assert!(matches!(
            serde_json::from_value::<crate::Pane>(value).unwrap(),
            crate::Pane::KlineChart {
                renko_state: None,
                ..
            }
        ));
    }
}
