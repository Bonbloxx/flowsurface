use super::{Action, Message, indicator::kline::KlineIndicatorImpl};
use crate::chart::indicator::kline::footprint_history::{
    DAYS, FootprintHistoryIndicator, missing_day_range, utc_day_ranges,
};
use crate::connector::fetcher::{
    FetchRange, FetchSpec, RequestHandler, TradeFetchMode, trade_fetch_mode,
};
use exchange::{
    OpenInterest, TickerInfo, Timeframe, Trade, UnixMs,
    adapter::{StreamKind, Venue},
    unit::PriceStep,
};
use iced::{Element, task::Handle};
use rustc_hash::{FxHashMap, FxHashSet};
use std::time::Instant;

const DAY_MS: u64 = 24 * 60 * 60 * 1_000;

pub struct FootprintHistory {
    indicator: FootprintHistoryIndicator,
    sources: Vec<TickerInfo>,
    aggregate: bool,
    block_step: PriceStep,
    cutoff: Option<UnixMs>,
    trade_requests: FxHashMap<uuid::Uuid, FootprintTradeRequest>,
    oi_requests: FxHashSet<uuid::Uuid>,
    request_handler: RequestHandler,
    fetch_handles: Vec<Handle>,
    last_invalidation: Instant,
}

#[derive(Clone, Copy)]
struct FootprintTradeRequest {
    source: TickerInfo,
    day_start: UnixMs,
    covered_through: UnixMs,
}

impl FootprintHistory {
    pub fn new(sources: Vec<TickerInfo>, aggregate: bool, block_step: PriceStep) -> Self {
        let mut indicator = FootprintHistoryIndicator::new();
        indicator.configure_footprint_history(&sources, aggregate);
        Self {
            indicator,
            sources,
            aggregate,
            block_step,
            cutoff: None,
            trade_requests: FxHashMap::default(),
            oi_requests: FxHashSet::default(),
            request_handler: RequestHandler::default(),
            fetch_handles: Vec::new(),
            last_invalidation: Instant::now(),
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        self.indicator.standalone_element(self.block_step)
    }

    pub fn sources(&self) -> &[TickerInfo] {
        &self.sources
    }

    pub fn aggregate(&self) -> bool {
        self.aggregate
    }

    pub fn block_step(&self) -> PriceStep {
        self.block_step
    }

    pub fn set_block_step(&mut self, block_step: PriceStep) {
        self.block_step = block_step;
        self.indicator.clear_all_caches();
    }

    pub fn configure_sources(&mut self, sources: Vec<TickerInfo>, aggregate: bool) {
        if sources.is_empty() {
            return;
        }
        if self.sources == sources && self.aggregate == aggregate {
            return;
        }
        self.sources = sources;
        self.aggregate = aggregate;
        // Keep completed request state and per-venue indicator history. Source
        // toggles should only recompose cached data, while newly-added venues
        // enqueue just their missing ranges on the next tick.
        self.indicator
            .configure_footprint_history(&self.sources, self.aggregate);
    }

    pub fn active_sources(&self) -> &[TickerInfo] {
        if self.aggregate {
            &self.sources
        } else {
            self.sources.get(..1).unwrap_or(&[])
        }
    }

    pub fn insert_live_trades(&mut self, source: TickerInfo, trades: &[Trade]) {
        self.indicator.on_source_trades(source, trades, false);
    }

    pub fn insert_historical_trades(
        &mut self,
        source: TickerInfo,
        trades: &[Trade],
        req_id: Option<uuid::Uuid>,
        batches_done: bool,
    ) -> bool {
        let Some(req_id) = req_id.filter(|id| self.trade_requests.contains_key(id)) else {
            return false;
        };
        self.indicator.on_source_trades(source, trades, true);
        if batches_done {
            if let Some(request) = self.trade_requests.remove(&req_id) {
                self.indicator.persist_cached_footprint_day(
                    request.source,
                    request.day_start,
                    request.covered_through,
                );
            }
            self.request_handler.mark_completed(req_id);
        }
        true
    }

    pub fn insert_open_interest(
        &mut self,
        source: TickerInfo,
        values: &[OpenInterest],
        req_id: Option<uuid::Uuid>,
    ) -> bool {
        let Some(req_id) = req_id.filter(|id| self.oi_requests.remove(id)) else {
            return false;
        };
        self.indicator.on_source_open_interest(source, values);
        if values.is_empty() {
            self.request_handler.mark_no_data(req_id);
        } else {
            self.request_handler.mark_completed(req_id);
        }
        true
    }

    pub fn set_handle(&mut self, handle: Handle) {
        self.fetch_handles.push(handle);
    }

    pub fn finalize_fetch(&mut self, req_id: uuid::Uuid) {
        if let Some(request) = self.trade_requests.remove(&req_id) {
            self.indicator.persist_cached_footprint_day(
                request.source,
                request.day_start,
                request.covered_through,
            );
        }
        self.oi_requests.remove(&req_id);
        self.request_handler.mark_completed(req_id);
    }

    pub fn mark_fetch_failed(&mut self, req_id: uuid::Uuid) {
        self.trade_requests.remove(&req_id);
        self.oi_requests.remove(&req_id);
        // Footprint history is a bounded snapshot. Do not turn a source error
        // into an invisible 30-second retry loop; a later explicit
        // reconfiguration or chart recreation can request it again.
        self.request_handler.mark_completed(req_id);
    }

    pub fn mark_fetch_no_data(&mut self, req_id: uuid::Uuid) {
        self.trade_requests.remove(&req_id);
        self.oi_requests.remove(&req_id);
        self.request_handler.mark_no_data(req_id);
    }

    pub fn last_update(&self) -> Instant {
        // This timestamp drives the pane's scheduled invalidation. It must not
        // follow live trade arrivals: an active market would continuously push
        // the deadline forward and prevent historical backfill from starting.
        self.last_invalidation
    }

    pub fn invalidate(&mut self, now: Option<Instant>) -> Option<Action> {
        self.indicator.clear_all_caches();
        let now = now?;
        self.last_invalidation = now;
        self.fetch_missing_data()
    }

    fn fetch_missing_data(&mut self) -> Option<Action> {
        let cutoff = *self.cutoff.get_or_insert_with(UnixMs::now);
        let today = cutoff.as_u64() / DAY_MS * DAY_MS;
        let fetch_mode = trade_fetch_mode();
        let mut specs = Vec::new();

        for source in self.active_sources().to_vec() {
            let trade_history_available = match fetch_mode {
                TradeFetchMode::Off => false,
                TradeFetchMode::Exchange => {
                    matches!(source.exchange().venue(), Venue::Binance | Venue::Bybit)
                }
                TradeFetchMode::Server => true,
            };

            if trade_history_available {
                self.indicator.prepare_footprint_history(source, cutoff);
                for (start, end) in utc_day_ranges(cutoff, DAYS as u64) {
                    let range = FetchRange::FootprintHistoryTrades(start, end);
                    let stream = StreamKind::Trades {
                        ticker_info: source,
                    };
                    if let Ok(Some(req_id)) = self.request_handler.add_request(range, Some(stream))
                    {
                        let cached_through =
                            self.indicator.load_cached_footprint_day(source, start);
                        if let Some((fetch_start, fetch_end)) =
                            missing_day_range(start, end, cached_through)
                        {
                            self.trade_requests.insert(
                                req_id,
                                FootprintTradeRequest {
                                    source,
                                    day_start: start,
                                    covered_through: end,
                                },
                            );
                            specs.push(FetchSpec {
                                req_id,
                                fetch: FetchRange::FootprintHistoryTrades(fetch_start, fetch_end),
                                stream: Some(stream),
                            });
                        } else {
                            self.request_handler.mark_completed(req_id);
                        }
                    }
                }
            }

            if matches!(source.exchange().venue(), Venue::Binance | Venue::Bybit)
                && source.is_perps()
            {
                let from =
                    UnixMs::new(today.saturating_sub((DAYS.saturating_sub(1) as u64) * DAY_MS));
                let range = FetchRange::FootprintHistoryOpenInterest(from, cutoff);
                let stream = StreamKind::Kline {
                    ticker_info: source,
                    timeframe: Timeframe::H1,
                };
                if let Ok(Some(req_id)) = self.request_handler.add_request(range, Some(stream)) {
                    self.oi_requests.insert(req_id);
                    specs.push(FetchSpec {
                        req_id,
                        fetch: range,
                        stream: Some(stream),
                    });
                }
            }
        }

        (!specs.is_empty()).then_some(Action::RequestFetch(specs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        Ticker, UnixMs,
        adapter::Exchange,
        unit::{Price, Qty},
    };

    fn ticker_info(exchange: Exchange) -> TickerInfo {
        TickerInfo::new(Ticker::new("BTCUSDT", exchange), 0.1, 0.001, None)
    }

    #[test]
    fn history_backfill_is_split_into_three_utc_days() {
        let cutoff = UnixMs::new(5 * DAY_MS + 1234);
        let ranges = utc_day_ranges(cutoff, DAYS as u64);
        assert_eq!(ranges.len(), 3);
        assert_eq!(
            ranges[0],
            (UnixMs::new(3 * DAY_MS), UnixMs::new(4 * DAY_MS - 1))
        );
        assert_eq!(
            ranges[1],
            (UnixMs::new(4 * DAY_MS), UnixMs::new(5 * DAY_MS - 1))
        );
        assert_eq!(ranges[2], (UnixMs::new(5 * DAY_MS), cutoff));
    }

    #[test]
    fn source_reconfiguration_preserves_completed_fetches() {
        let binance = ticker_info(Exchange::BinanceLinear);
        let bybit = ticker_info(Exchange::BybitLinear);
        let mut history = FootprintHistory::new(
            vec![binance, bybit],
            true,
            PriceStep::from(binance.min_ticksize),
        );
        let range = FetchRange::FootprintHistoryTrades(UnixMs::new(1), UnixMs::new(2));
        let stream = StreamKind::Trades { ticker_info: bybit };
        let req_id = history
            .request_handler
            .add_request(range, Some(stream))
            .expect("valid request")
            .expect("new request");
        history.request_handler.mark_completed(req_id);

        history.configure_sources(vec![binance], true);
        history.configure_sources(vec![binance, bybit], true);

        assert!(
            history
                .request_handler
                .add_request(range, Some(stream))
                .expect("completed request")
                .is_none()
        );
    }

    #[test]
    fn live_trades_do_not_postpone_historical_backfill_tick() {
        let source = ticker_info(Exchange::BinanceLinear);
        let mut history =
            FootprintHistory::new(vec![source], false, PriceStep::from(source.min_ticksize));
        let scheduled_invalidation = history.last_update();

        history.insert_live_trades(
            source,
            &[Trade {
                time: UnixMs::now(),
                is_sell: false,
                price: Price::from_f64(70_000.0),
                qty: Qty::from_f64(1.0),
            }],
        );

        assert_eq!(history.last_update(), scheduled_invalidation);
    }
}
