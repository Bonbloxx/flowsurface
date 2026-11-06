use super::{TickAccumulation, TickAggr};
use crate::chart::{
    kline::{GroupedTrades, NPoc, PointOfControl},
    trev::{Config, Display},
};
use exchange::{
    Ticker, Trade, UnixMs, Volume,
    unit::{Price, PriceStep, Qty},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct State {
    config: Config,
    direction: Option<Direction>,
    last_time: Option<UnixMs>,
    last_trades: Vec<Print>,
    #[serde(skip)]
    trimmed: bool,
    // Derived runtime accounting, never persisted. Avoid scanning every retained
    // footprint on each live batch just to enforce the history budget.
    #[serde(skip)]
    retained_rows: usize,
    #[serde(skip)]
    needs_poc: bool,
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
            units: Price::from_f64(1.0).units,
        }
    }
    fn config() -> Config {
        Config {
            continuation_ticks: 10,
            reversal_ticks: 15,
            ticks_per_row: 1,
            ..Default::default()
        }
    }
    fn trade(time: u64, price: f64) -> Trade {
        Trade {
            time: UnixMs::new(time),
            price: Price::from_f64(price),
            qty: Qty::from_f64(2.0),
            is_sell: time.is_multiple_of(2),
        }
    }
    fn saved(aggr: &TickAggr) -> String {
        serde_json::to_string(&aggr.trev_snapshot(ticker())).unwrap()
    }
    fn tape() -> Vec<Trade> {
        [
            100., 104., 98., 110., 113., 120., 117., 109., 105., 100., 95., 90., 99., 105., 110.,
            115.,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, p)| trade(i as u64 / 2, p))
        .collect()
    }

    #[test]
    fn independent_continuation_and_reversal_thresholds() {
        let input = [100., 109., 110., 120., 106., 105., 95., 109., 110.];
        let trades: Vec<_> = input
            .into_iter()
            .enumerate()
            .map(|(i, p)| trade(i as u64, p))
            .collect();
        let aggr = TickAggr::new_trev(config(), step(), &trades);
        let bodies: Vec<_> = aggr
            .datapoints
            .iter()
            .map(|bar| (bar.kline.open.to_f64(), bar.kline.close.to_f64()))
            .collect();
        assert_eq!(
            bodies,
            [
                (100., 110.),
                (110., 120.),
                (120., 105.),
                (105., 95.),
                (95., 110.),
                (110., 110.)
            ]
        );
        let short_reverse = Config {
            reversal_ticks: 6,
            ..config()
        };
        let aggr = TickAggr::new_trev(
            short_reverse,
            step(),
            &[trade(0, 100.), trade(1, 110.), trade(2, 104.)],
        );
        assert_eq!(aggr.datapoints.len(), 3);
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f64(110.));
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(104.));
    }

    #[test]
    fn replay_chunk_boundaries_and_equal_millisecond_restart_preserve_exact_cells() {
        let tape = tape();
        let expected = saved(&TickAggr::new_trev(config(), step(), &tape));
        for split in 1..tape.len() {
            let first = TickAggr::new_trev(config(), step(), &tape[..split]);
            let snapshot: Snapshot = serde_json::from_str(
                &serde_json::to_string(&first.trev_snapshot(ticker()).unwrap()).unwrap(),
            )
            .unwrap();
            let mut restored =
                TickAggr::restore_trev(&snapshot, ticker(), config(), step()).unwrap();
            restored.insert_trades(&tape[split..]);
            assert_eq!(saved(&restored), expected, "split {split}");
            let mut chunked = first;
            chunked.insert_trades(&tape[split..]);
            assert_eq!(saved(&chunked), expected);
        }
    }

    #[test]
    fn closing_print_and_price_jump_never_duplicate_or_invent_volume() {
        let trades = [
            trade(0, 100.),
            trade(1, 110.),
            trade(2, 180.),
            trade(3, 161.),
        ];
        let aggr = TickAggr::new_trev(config(), step(), &trades);
        assert_eq!(aggr.datapoints.len(), 4); // one closing bar per real print
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(180.));
        assert_eq!(
            aggr.datapoints
                .iter()
                .map(|bar| bar.tick_count)
                .sum::<usize>(),
            trades.len()
        );
        let rows: Vec<_> = aggr
            .datapoints
            .iter()
            .flat_map(|bar| bar.footprint.trades.values())
            .collect();
        assert_eq!(
            rows.iter()
                .map(|row| row.buy_qty.units + row.sell_qty.units)
                .sum::<i64>(),
            Qty::from_f64(8.).units
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.buy_count + row.sell_count)
                .sum::<usize>(),
            trades.len()
        );
        assert!(
            aggr.datapoints[1]
                .footprint
                .trades
                .keys()
                .all(|price| *price == Price::from_f64(180.))
        );
    }

    #[test]
    fn row_aggregation_and_display_have_no_effect_on_bar_construction() {
        let tape = tape();
        let mut fine = TickAggr::new_trev(config(), step(), &tape);
        let coarse = TickAggr::new_trev(
            Config {
                ticks_per_row: 100,
                display: crate::chart::trev::Display::Delta,
                ..config()
            },
            step(),
            &tape,
        );
        for (a, b) in fine.datapoints.iter().zip(&coarse.datapoints) {
            assert_eq!(
                (
                    a.kline.open,
                    a.kline.close,
                    a.kline.high,
                    a.kline.low,
                    a.tick_count
                ),
                (
                    b.kline.open,
                    b.kline.close,
                    b.kline.high,
                    b.kline.low,
                    b.tick_count
                )
            );
            assert_eq!(a.footprint.trades.len(), b.footprint.trades.len());
        }
        let snapshot = fine.trev_snapshot(ticker()).unwrap();
        assert!(
            TickAggr::restore_trev(
                &snapshot,
                ticker(),
                Config {
                    ticks_per_row: 500,
                    ..config()
                },
                step()
            )
            .is_some()
        );
        let untouched = saved(&coarse);
        fine.insert_trades(&[trade(10, 140.)]);
        assert_eq!(saved(&coarse), untouched);
    }

    #[test]
    fn minimum_lifetime_preserves_excursions_and_all_executions() {
        let config = Config {
            normalization_ms: 1_000,
            ..config()
        };
        let aggr = TickAggr::new_trev(
            config,
            step(),
            &[
                trade(0, 100.),
                trade(1, 140.),
                trade(500, 90.),
                trade(1000, 120.),
            ],
        );
        assert_eq!(aggr.datapoints.len(), 2);
        let bar = &aggr.datapoints[0];
        assert_eq!(
            (bar.kline.high, bar.kline.low, bar.kline.close),
            (
                Price::from_f64(140.),
                Price::from_f64(90.),
                Price::from_f64(120.)
            )
        );
        assert_eq!(bar.tick_count, 4);
    }

    #[test]
    fn gap_starts_a_new_segment_and_preserves_partial_bar_volume() {
        let mut aggr = TickAggr::new_trev(
            config(),
            step(),
            &[trade(0, 100.), trade(1, 110.), trade(2, 114.)],
        );
        aggr.resume_trev_after_gap(&[trade(100, 200.), trade(101, 210.)]);
        assert_eq!(
            aggr.datapoints.iter().map(|b| b.tick_count).sum::<usize>(),
            5
        );
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f64(114.));
        assert_eq!(aggr.datapoints[2].kline.open, Price::from_f64(200.));
    }

    #[test]
    fn retention_preserves_the_developing_anchor_and_is_bounded() {
        let mut aggr = TickAggr::new_trev(
            config(),
            step(),
            &[trade(0, 100.), trade(1, 110.), trade(2, 114.)],
        );
        aggr.trim_trev_history(UnixMs::new(TickAggr::TREV_HISTORY_WINDOW_MS + 10));
        assert_eq!(aggr.datapoints.len(), 1);
        assert_eq!(aggr.datapoints[0].kline.open, Price::from_f64(110.));
        assert_eq!(aggr.datapoints[0].tick_count, 1);
        let many: Vec<_> = (0..TickAggr::MAX_TREV_BARS + 100)
            .map(|i| trade(i as u64, 100. + 10. * i as f64))
            .collect();
        let bounded = TickAggr::new_trev(config(), step(), &many);
        assert!(bounded.datapoints.len() <= TickAggr::MAX_TREV_BARS + 1);
    }

    #[test]
    fn display_poc_and_cached_row_budget_match_full_exact_books() {
        let mut aggr = TickAggr::new_trev(config(), step(), &tape());
        assert!(aggr.datapoints.iter().all(|b| b.footprint.poc.is_none()));
        aggr.set_trev_display(Display::BidAsk);
        aggr.insert_trades(&[trade(20, 115.), trade(21, 115.), trade(22, 130.)]);
        let check = |aggr: &TickAggr| {
            assert_eq!(
                aggr.trev.as_ref().unwrap().retained_rows,
                aggr.datapoints
                    .iter()
                    .map(|b| b.footprint.trades.len())
                    .sum::<usize>()
            );
            for bar in &aggr.datapoints {
                let maximum = bar
                    .footprint
                    .trades
                    .values()
                    .map(|row| row.total_qty())
                    .max();
                let expected = maximum.and_then(|maximum| {
                    bar.footprint
                        .trades
                        .iter()
                        .filter(|(_, row)| row.total_qty() == maximum)
                        .map(|(price, _)| *price)
                        .min()
                });
                assert_eq!(bar.poc_price(), expected);
            }
        };
        check(&aggr);
        let saved: Snapshot = serde_json::from_str(
            &serde_json::to_string(&aggr.trev_snapshot(ticker()).unwrap()).unwrap(),
        )
        .unwrap();
        let mut restored = TickAggr::restore_trev(
            &saved,
            ticker(),
            Config {
                display: Display::BidAsk,
                ..config()
            },
            step(),
        )
        .unwrap();
        check(&restored);
        restored.resume_trev_after_gap(&[trade(30, 200.), trade(31, 210.)]);
        check(&restored);
        restored.trim_trev_history(UnixMs::new(TickAggr::TREV_HISTORY_WINDOW_MS + 29));
        check(&restored);
    }

    #[test]
    fn incompatible_or_corrupt_snapshots_are_rejected() {
        let aggr = TickAggr::new_trev(config(), step(), &tape());
        let mut snapshot = aggr.trev_snapshot(ticker()).unwrap();
        assert!(
            TickAggr::restore_trev(
                &snapshot,
                ticker(),
                Config {
                    continuation_ticks: 11,
                    ..config()
                },
                step()
            )
            .is_none()
        );
        assert!(
            TickAggr::restore_trev(
                &snapshot,
                Ticker::new("ETHUSDT", Exchange::BinanceLinear),
                config(),
                step()
            )
            .is_none()
        );
        assert!(
            TickAggr::restore_trev(
                &snapshot,
                ticker(),
                config(),
                PriceStep {
                    units: Price::from_f64(0.1).units
                }
            )
            .is_none()
        );
        snapshot.bars[0].buy += Qty::from_f64(1.);
        assert!(TickAggr::restore_trev(&snapshot, ticker(), config(), step()).is_none());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Print {
    time: UnixMs,
    price: Price,
    qty: Qty,
    is_sell: bool,
}

impl From<&Trade> for Print {
    fn from(t: &Trade) -> Self {
        Self {
            time: t.time,
            price: t.price,
            qty: t.qty,
            is_sell: t.is_sell,
        }
    }
}

impl From<&Print> for Trade {
    fn from(t: &Print) -> Self {
        Self {
            time: t.time,
            price: t.price,
            qty: t.qty,
            is_sell: t.is_sell,
        }
    }
}

/// Separate from Renko's snapshot: TRev preserves its exact price-level book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    version: u8,
    ticker: Ticker,
    step: PriceStep,
    state: State,
    bars: Vec<Bar>,
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
    rows: Vec<(Price, Row)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Row {
    buy: Qty,
    sell: Qty,
    first: UnixMs,
    last: UnixMs,
    buys: usize,
    sells: usize,
}

impl TickAggr {
    pub const TREV_HISTORY_WINDOW_MS: u64 = 4 * 60 * 60 * 1_000;
    pub const MAX_TREV_BARS: usize = 10_000;
    pub const MAX_TREV_ROWS: usize = 250_000;

    pub fn new_trev(config: Config, step: PriceStep, trades: &[Trade]) -> Self {
        let mut aggr = Self::new(crate::aggr::TickCount(1), step, &[]);
        aggr.trev = Some(Box::new(State {
            config: config.normalized(),
            direction: None,
            last_time: None,
            last_trades: Vec::new(),
            trimmed: false,
            retained_rows: 0,
            needs_poc: config.display.clusters().is_some(),
        }));
        aggr.insert_trev_trades(trades);
        aggr
    }

    pub fn is_trev(&self) -> bool {
        self.trev.is_some()
    }

    pub fn trev_last_input_time(&self) -> Option<UnixMs> {
        self.trev.as_ref().and_then(|state| state.last_time)
    }

    pub fn trev_boundary_trades(&self) -> Vec<Trade> {
        self.trev.as_ref().map_or_else(Vec::new, |state| {
            state.last_trades.iter().map(Trade::from).collect()
        })
    }

    pub fn trev_trimmed_on_last_insert(&self) -> bool {
        self.trev.as_ref().is_some_and(|state| state.trimmed)
    }

    pub(super) fn insert_trev_trades(&mut self, trades: &[Trade]) {
        if !self.is_trev() || trades.is_empty() {
            return;
        }
        // Stable sorting preserves exchange order within the same millisecond.
        let ordered = (!trades.windows(2).all(|w| w[0].time <= w[1].time)).then(|| {
            let mut ordered = trades.to_vec();
            ordered.sort_by_key(|t| t.time);
            ordered
        });
        for trade in ordered.as_deref().unwrap_or(trades) {
            let state = self.trev.as_mut().expect("TRev initialized");
            if state.last_time.is_some_and(|last| trade.time < last) {
                continue;
            }
            if state.last_time != Some(trade.time) {
                state.last_trades.clear();
            }
            state.last_time = Some(trade.time);
            state.last_trades.push(Print::from(trade));
            if self.datapoints.is_empty() {
                let mut bar = TickAccumulation::new(trade, self.tick_size);
                if state.needs_poc {
                    update_poc(&mut bar, trade.price.round_to_step(self.tick_size));
                }
                state.retained_rows += bar.footprint.trades.len();
                self.datapoints.push(bar);
                continue;
            }
            let bar = self.datapoints.last_mut().expect("TRev bar initialized");
            let rows_before = bar.footprint.trades.len();
            bar.update_with_trade(trade, self.tick_size);
            state.retained_rows += bar.footprint.trades.len() - rows_before;
            if state.needs_poc {
                if trade.qty.units < 0 {
                    rebuild_poc(bar);
                } else {
                    update_poc(bar, trade.price.round_to_step(self.tick_size));
                }
            }
            if trade.time.saturating_diff(bar.kline.time) < u64::from(state.config.normalization_ms)
            {
                continue;
            }
            let movement = i128::from(trade.price.units) - i128::from(bar.kline.open.units);
            let trend =
                i128::from(self.tick_size.units) * i128::from(state.config.continuation_ticks);
            let reverse =
                i128::from(self.tick_size.units) * i128::from(state.config.reversal_ticks);
            if trend <= 0 || reverse <= 0 {
                continue;
            }
            let next = match state.direction {
                None if movement >= trend => Some(Direction::Up),
                None if movement <= -trend => Some(Direction::Down),
                Some(Direction::Up) if movement >= trend => Some(Direction::Up),
                Some(Direction::Up) if movement <= -reverse => Some(Direction::Down),
                Some(Direction::Down) if movement <= -trend => Some(Direction::Down),
                Some(Direction::Down) if movement >= reverse => Some(Direction::Up),
                _ => None,
            };
            if let Some(direction) = next {
                // The triggering execution belongs wholly to the closing bar.
                // Retain its real price on overshoot; never fabricate gap prints
                // or split/repeat its quantity across synthetic bars.
                state.direction = Some(direction);
                self.datapoints.push(TickAccumulation::empty_at(
                    trade.time,
                    trade.price,
                    trade.price,
                ));
            }
        }
        if let Some(last) = self.trev_last_input_time() {
            self.trim_trev_history(last);
        }
    }

    pub fn trim_trev_history(&mut self, now: UnixMs) -> bool {
        let Some(state) = self.trev.as_mut() else {
            return false;
        };
        let cutoff = now.saturating_sub(Self::TREV_HISTORY_WINDOW_MS);
        // Retain an old developing bar: its anchor/footprint cannot be discarded
        // merely because price has stayed in range for more than four hours.
        let confirmed = self.datapoints.len().saturating_sub(1);
        let expired = self.datapoints[..confirmed].partition_point(|bar| bar.kline.time < cutoff);
        let mut remove = expired.max(
            self.datapoints
                .len()
                .saturating_sub(Self::MAX_TREV_BARS + 1),
        );
        let removed_rows: usize = self.datapoints[..remove]
            .iter()
            .map(|bar| bar.footprint.trades.len())
            .sum();
        let mut rows = state.retained_rows - removed_rows;
        while rows > Self::MAX_TREV_ROWS && remove < confirmed {
            rows -= self.datapoints[remove].footprint.trades.len();
            remove += 1;
        }
        self.datapoints.drain(..remove);
        state.retained_rows = rows;
        state.trimmed = remove > 0;
        state.trimmed
    }

    /// Presentation changes never rebuild the price chain. POC is only needed
    /// for footprint displays and is maintained incrementally after this scan.
    pub fn set_trev_display(&mut self, display: Display) {
        let Some(state) = self.trev.as_mut() else {
            return;
        };
        let needs_poc = display.clusters().is_some();
        if needs_poc && !state.needs_poc {
            self.datapoints.iter_mut().for_each(rebuild_poc);
        }
        state.needs_poc = needs_poc;
    }

    pub fn resume_trev_after_gap(&mut self, trades: &[Trade]) {
        let Some(state) = self.trev.as_ref() else {
            return;
        };
        if trades.is_empty() {
            return;
        }
        let mut resumed = Self::new_trev(state.config, self.tick_size, &[]);
        resumed.trev.as_mut().expect("TRev resumed").needs_poc = state.needs_poc;
        resumed.insert_trev_trades(trades);
        // Preserve observed volume in a partial pre-gap bar as well.
        if self
            .datapoints
            .last()
            .is_some_and(|bar| bar.tick_count == 0)
        {
            self.datapoints.pop();
        }
        self.datapoints.append(&mut resumed.datapoints);
        self.trev = resumed.trev;
        self.trev.as_mut().expect("TRev resumed").retained_rows = self
            .datapoints
            .iter()
            .map(|bar| bar.footprint.trades.len())
            .sum();
        if let Some(last) = self.trev_last_input_time() {
            self.trim_trev_history(last);
        }
    }

    pub fn trev_snapshot(&self, ticker: Ticker) -> Option<Box<Snapshot>> {
        let state = self.trev.as_ref()?;
        state.last_time?;
        Some(Box::new(Snapshot {
            version: 1,
            ticker,
            step: self.tick_size,
            state: *state.clone(),
            bars: self
                .datapoints
                .iter()
                .map(|bar| {
                    let mut rows: Vec<_> = bar
                        .footprint
                        .trades
                        .iter()
                        .map(|(price, row)| {
                            (
                                *price,
                                Row {
                                    buy: row.buy_qty,
                                    sell: row.sell_qty,
                                    first: row.first_time,
                                    last: row.last_time,
                                    buys: row.buy_count,
                                    sells: row.sell_count,
                                },
                            )
                        })
                        .collect();
                    rows.sort_by_key(|(price, _)| *price);
                    Bar {
                        time: bar.kline.time,
                        open: bar.kline.open,
                        high: bar.kline.high,
                        low: bar.kline.low,
                        close: bar.kline.close,
                        buy: bar.kline.volume.buy_qty_or_zero(),
                        sell: bar.kline.volume.sell_qty_or_zero(),
                        count: bar.tick_count,
                        rows,
                    }
                })
                .collect(),
        }))
    }

    pub fn restore_trev(
        snapshot: &Snapshot,
        ticker: Ticker,
        config: Config,
        step: PriceStep,
    ) -> Option<Self> {
        let last = snapshot.state.last_time?;
        if snapshot.version != 1
            || !snapshot.ticker.same_market(&ticker)
            || snapshot.step != step
            || !snapshot.state.config.same_construction(config.normalized())
            || step.units <= 0
            || snapshot.bars.is_empty()
            || snapshot.bars.len() > Self::MAX_TREV_BARS + 1
            || !snapshot.bars.windows(2).all(|w| w[0].time <= w[1].time)
            || snapshot.bars.iter().any(|bar| {
                let buy: i128 = bar
                    .rows
                    .iter()
                    .map(|(_, row)| i128::from(row.buy.units))
                    .sum();
                let sell: i128 = bar
                    .rows
                    .iter()
                    .map(|(_, row)| i128::from(row.sell.units))
                    .sum();
                let count: u128 = bar
                    .rows
                    .iter()
                    .map(|(_, row)| row.buys as u128 + row.sells as u128)
                    .sum();
                bar.time > last
                    || bar.low > bar.open.min(bar.close)
                    || bar.high < bar.open.max(bar.close)
                    || buy != i128::from(bar.buy.units)
                    || sell != i128::from(bar.sell.units)
                    || count != bar.count as u128
                    || !bar.rows.windows(2).all(|w| w[0].0 < w[1].0)
                    || bar.rows.iter().any(|(price, row)| {
                        row.buy.units < 0
                            || row.sell.units < 0
                            || *price < bar.low
                            || *price > bar.high
                            || row.first < bar.time
                            || row.first > row.last
                            || row.last > last
                    })
            })
            || snapshot
                .state
                .last_trades
                .iter()
                .any(|t| t.time != last || t.qty.units < 0)
        {
            return None;
        }
        let mut aggr = Self::new_trev(config, step, &[]);
        aggr.trev = Some(Box::new(State {
            config: config.normalized(),
            trimmed: false,
            needs_poc: config.display.clusters().is_some(),
            ..snapshot.state.clone()
        }));
        aggr.datapoints = snapshot
            .bars
            .iter()
            .map(|bar| {
                let mut dp = TickAccumulation::empty_at(bar.time, bar.open, bar.close);
                dp.kline.high = bar.high;
                dp.kline.low = bar.low;
                dp.kline.volume = Volume::BuySell(bar.buy, bar.sell);
                dp.tick_count = bar.count;
                dp.footprint.trades = bar
                    .rows
                    .iter()
                    .map(|(price, row)| {
                        (
                            *price,
                            GroupedTrades {
                                buy_qty: row.buy,
                                sell_qty: row.sell,
                                first_time: row.first,
                                last_time: row.last,
                                buy_count: row.buys,
                                sell_count: row.sells,
                            },
                        )
                    })
                    .collect();
                if config.display.clusters().is_some() {
                    rebuild_poc(&mut dp);
                }
                dp
            })
            .collect();
        aggr.trev.as_mut().expect("TRev restored").retained_rows = aggr
            .datapoints
            .iter()
            .map(|bar| bar.footprint.trades.len())
            .sum();
        Some(aggr)
    }
}

// TRev resolves equal-volume POCs at the lower price. This makes its incremental
// and restored books agree without depending on hash-map iteration order.
fn update_poc(bar: &mut TickAccumulation, price: Price) {
    let volume = bar.footprint.trades[&price].total_qty();
    if bar
        .footprint
        .poc
        .is_none_or(|poc| volume > poc.volume || (volume == poc.volume && price < poc.price))
    {
        bar.footprint.poc = Some(PointOfControl {
            price,
            volume,
            status: NPoc::default(),
        });
    } else if let Some(poc) = &mut bar.footprint.poc
        && poc.price == price
    {
        poc.volume = volume;
    }
}

fn rebuild_poc(bar: &mut TickAccumulation) {
    bar.footprint.poc = bar
        .footprint
        .trades
        .iter()
        .max_by(|(a_price, a), (b_price, b)| {
            a.total_qty()
                .cmp(&b.total_qty())
                .then_with(|| b_price.cmp(a_price))
        })
        .map(|(price, row)| PointOfControl {
            price: *price,
            volume: row.total_qty(),
            status: NPoc::default(),
        });
}
