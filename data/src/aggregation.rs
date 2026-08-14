use exchange::{
    Kline, Ticker, TickerInfo, Timeframe, Volume,
    adapter::{Exchange, StreamKind},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable identity for a logical market-data feed.
///
/// Consumers depend on this identity rather than on individual venues. Adding a
/// venue therefore changes the catalog below, not every chart or indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum AggregateFeedId {
    BtcUsdtPerpetual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceDefinition {
    pub exchange: Exchange,
    pub symbol: &'static str,
    pub label: &'static str,
}

impl SourceDefinition {
    pub fn ticker(self) -> Ticker {
        Ticker::new(self.symbol, self.exchange)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedDefinition {
    pub id: AggregateFeedId,
    pub label: &'static str,
    pub sources: &'static [SourceDefinition],
}

const BTCUSDT_PERPETUAL_SOURCES: &[SourceDefinition] = &[
    SourceDefinition {
        exchange: Exchange::BinanceLinear,
        symbol: "BTCUSDT",
        label: "Binance",
    },
    SourceDefinition {
        exchange: Exchange::BybitLinear,
        symbol: "BTCUSDT",
        label: "Bybit",
    },
    SourceDefinition {
        exchange: Exchange::HyperliquidLinear,
        symbol: "BTC",
        label: "Hyperliquid",
    },
];

const BTCUSDT_PERPETUAL: FeedDefinition = FeedDefinition {
    id: AggregateFeedId::BtcUsdtPerpetual,
    label: "BTCUSDT Perpetual",
    sources: BTCUSDT_PERPETUAL_SOURCES,
};

impl AggregateFeedId {
    pub fn definition(self) -> &'static FeedDefinition {
        match self {
            Self::BtcUsdtPerpetual => &BTCUSDT_PERPETUAL,
        }
    }

    pub fn for_seed_ticker(ticker: Ticker) -> Option<Self> {
        [Self::BtcUsdtPerpetual].into_iter().find(|feed| {
            feed.definition()
                .sources
                .iter()
                .any(|source| source.ticker() == ticker)
        })
    }

    pub fn source_tickers(self) -> impl Iterator<Item = Ticker> {
        self.definition()
            .sources
            .iter()
            .map(|source| source.ticker())
    }

    pub fn source_label(self, ticker: Ticker) -> Option<&'static str> {
        self.definition()
            .sources
            .iter()
            .find(|source| source.ticker() == ticker)
            .map(|source| source.label)
    }
}

/// Runtime feed resolved from exchange metadata.
#[derive(Debug, Clone)]
pub struct ResolvedFeed {
    id: Option<AggregateFeedId>,
    available_sources: Vec<TickerInfo>,
    sources: Vec<TickerInfo>,
}

impl ResolvedFeed {
    pub fn direct(source: TickerInfo) -> Self {
        Self {
            id: None,
            available_sources: vec![source],
            sources: vec![source],
        }
    }

    pub fn aggregated(id: AggregateFeedId, primary: TickerInfo, candidates: &[TickerInfo]) -> Self {
        Self::aggregated_selected(id, primary, candidates, None)
    }

    pub fn aggregated_selected(
        id: AggregateFeedId,
        primary: TickerInfo,
        candidates: &[TickerInfo],
        selected: Option<&[Ticker]>,
    ) -> Self {
        let mut available_sources = Vec::new();
        for wanted in id.source_tickers() {
            if let Some(info) = candidates
                .iter()
                .copied()
                .find(|candidate| candidate.ticker == wanted)
                .or_else(|| (primary.ticker == wanted).then_some(primary))
            {
                available_sources.push(info);
            }
        }

        if available_sources.is_empty() {
            available_sources.push(primary);
        }

        let mut sources = available_sources
            .iter()
            .copied()
            .filter(|source| selected.is_none_or(|wanted| wanted.contains(&source.ticker)))
            .collect::<Vec<_>>();

        if sources.is_empty() {
            sources.push(primary);
        }

        Self {
            id: Some(id),
            available_sources,
            sources,
        }
    }

    pub fn id(&self) -> Option<AggregateFeedId> {
        self.id
    }

    pub fn primary(&self) -> TickerInfo {
        self.sources[0]
    }

    pub fn sources(&self) -> &[TickerInfo] {
        &self.sources
    }

    pub fn available_sources(&self) -> &[TickerInfo] {
        &self.available_sources
    }

    /// Returns the next non-empty source selection in catalog order.
    /// `None` means the requested toggle would disable the final source.
    pub fn toggled_source_tickers(&self, ticker: Ticker, enabled: bool) -> Option<Vec<Ticker>> {
        if !self
            .available_sources
            .iter()
            .any(|source| source.ticker == ticker)
        {
            return None;
        }

        let selected = self
            .available_sources
            .iter()
            .filter_map(|source| {
                let is_selected = self.sources.contains(source);
                let keep = if source.ticker == ticker {
                    enabled
                } else {
                    is_selected
                };
                keep.then_some(source.ticker)
            })
            .collect::<Vec<_>>();

        (!selected.is_empty()).then_some(selected)
    }

    pub fn trade_streams(&self) -> Vec<StreamKind> {
        self.sources
            .iter()
            .copied()
            .map(|ticker_info| StreamKind::Trades { ticker_info })
            .collect()
    }

    pub fn kline_streams(&self, timeframe: Timeframe) -> Vec<StreamKind> {
        self.sources
            .iter()
            .copied()
            .map(|ticker_info| StreamKind::Kline {
                ticker_info,
                timeframe,
            })
            .collect()
    }
}

/// Source-aware historical bar store with deterministic composite output.
///
/// The first source in the feed is authoritative for open/close. Every source
/// contributes to high, low, and volume. This keeps a stable displayed price
/// while preserving the full traded range used by TPO and future tools.
#[derive(Debug, Clone)]
pub struct KlineAggregator {
    sources: Vec<TickerInfo>,
    bars: FxHashMap<TickerInfo, BTreeMap<exchange::UnixMs, Kline>>,
    exhausted: FxHashSet<TickerInfo>,
}

impl KlineAggregator {
    pub fn new(feed: &ResolvedFeed) -> Self {
        Self {
            sources: feed.sources.clone(),
            bars: FxHashMap::default(),
            exhausted: FxHashSet::default(),
        }
    }

    pub fn insert(&mut self, source: TickerInfo, klines: &[Kline]) {
        if !self.sources.contains(&source) {
            return;
        }

        let source_bars = self.bars.entry(source).or_default();
        for kline in klines {
            source_bars.insert(kline.time, *kline);
        }
    }

    pub fn mark_exhausted(&mut self, source: TickerInfo) {
        if self.sources.contains(&source) {
            self.exhausted.insert(source);
        }
    }

    pub fn earliest(&self, source: TickerInfo) -> Option<exchange::UnixMs> {
        self.bars
            .get(&source)
            .and_then(|bars| bars.first_key_value().map(|(time, _)| *time))
    }

    pub fn source_is_complete(&self, source: TickerInfo, need_earliest: exchange::UnixMs) -> bool {
        self.exhausted.contains(&source)
            || self
                .earliest(source)
                .is_some_and(|earliest| earliest <= need_earliest)
    }

    pub fn all_sources_complete(&self, need_earliest: exchange::UnixMs) -> bool {
        self.sources
            .iter()
            .copied()
            .all(|source| self.source_is_complete(source, need_earliest))
    }

    pub fn composite_klines(&self) -> Vec<Kline> {
        let mut composite = BTreeMap::<exchange::UnixMs, Kline>::new();

        // Source order is catalog priority. The primary source establishes
        // open/close; later sources expand the envelope and add volume.
        for source in &self.sources {
            let Some(source_bars) = self.bars.get(source) else {
                continue;
            };

            for (time, incoming) in source_bars {
                match composite.entry(*time) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(*incoming);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        let current = entry.get_mut();
                        current.high = current.high.max(incoming.high);
                        current.low = current.low.min(incoming.low);
                        current.volume = merge_volume(current.volume, incoming.volume);
                    }
                }
            }
        }

        composite.into_values().collect()
    }
}

fn merge_volume(left: Volume, right: Volume) -> Volume {
    match (left.buy_sell(), right.buy_sell()) {
        (Some((left_buy, left_sell)), Some((right_buy, right_sell))) => {
            Volume::BuySell(left_buy + right_buy, left_sell + right_sell)
        }
        _ => Volume::TotalOnly(left.total() + right.total()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{
        UnixMs,
        unit::{Price, Qty},
    };

    fn ticker_info(exchange: Exchange, symbol: &str) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), 0.1, 0.001, None)
    }

    fn kline(time: u64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Kline {
        Kline {
            time: UnixMs::new(time),
            open: Price::from_f64(open),
            high: Price::from_f64(high),
            low: Price::from_f64(low),
            close: Price::from_f64(close),
            volume: Volume::TotalOnly(Qty::from_f64(volume)),
        }
    }

    #[test]
    fn btc_perpetual_starts_with_binance_linear() {
        let definition = AggregateFeedId::BtcUsdtPerpetual.definition();
        assert_eq!(definition.sources, BTCUSDT_PERPETUAL_SOURCES);
        assert_eq!(
            definition.sources[0].ticker(),
            Ticker::new("BTCUSDT", Exchange::BinanceLinear)
        );
        assert_eq!(
            AggregateFeedId::for_seed_ticker(definition.sources[0].ticker()),
            Some(AggregateFeedId::BtcUsdtPerpetual)
        );
        assert_eq!(
            definition.sources[1].ticker(),
            Ticker::new("BTCUSDT", Exchange::BybitLinear)
        );
        assert_eq!(
            definition.sources[2].ticker(),
            Ticker::new("BTC", Exchange::HyperliquidLinear)
        );
        assert!(definition.sources.iter().all(|source| {
            AggregateFeedId::for_seed_ticker(source.ticker())
                == Some(AggregateFeedId::BtcUsdtPerpetual)
        }));
    }

    #[test]
    fn btc_perpetual_resolves_all_catalog_sources_in_priority_order() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC");

        let feed = ResolvedFeed::aggregated(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &[hyperliquid, binance, bybit],
        );

        assert_eq!(feed.sources(), &[binance, bybit, hyperliquid]);
        assert_eq!(feed.trade_streams().len(), 3);
        assert_eq!(feed.kline_streams(Timeframe::M1).len(), 3);
    }

    #[test]
    fn btc_perpetual_supports_independent_and_mixed_source_selections() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC");
        let candidates = [hyperliquid, binance, bybit];

        let bybit_only = ResolvedFeed::aggregated_selected(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &candidates,
            Some(&[bybit.ticker]),
        );
        assert_eq!(bybit_only.sources(), &[bybit]);
        assert_eq!(
            bybit_only.available_sources(),
            &[binance, bybit, hyperliquid]
        );
        assert_eq!(bybit_only.trade_streams().len(), 1);

        let bybit_hyperliquid = ResolvedFeed::aggregated_selected(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &candidates,
            Some(&[bybit.ticker, hyperliquid.ticker]),
        );
        assert_eq!(bybit_hyperliquid.sources(), &[bybit, hyperliquid]);
        assert_eq!(bybit_hyperliquid.trade_streams().len(), 2);
    }

    #[test]
    fn source_toggles_keep_at_least_one_source_enabled() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC");
        let candidates = [binance, bybit, hyperliquid];
        let feed = ResolvedFeed::aggregated_selected(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &candidates,
            Some(&[bybit.ticker]),
        );

        assert_eq!(feed.toggled_source_tickers(bybit.ticker, false), None);
        assert_eq!(
            feed.toggled_source_tickers(hyperliquid.ticker, true),
            Some(vec![bybit.ticker, hyperliquid.ticker])
        );
    }

    #[test]
    fn composite_keeps_primary_open_close_and_merges_range_and_volume() {
        let primary = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let secondary = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let feed = ResolvedFeed {
            id: Some(AggregateFeedId::BtcUsdtPerpetual),
            available_sources: vec![primary, secondary],
            sources: vec![primary, secondary],
        };
        let mut aggregator = KlineAggregator::new(&feed);

        aggregator.insert(primary, &[kline(1_000, 100.0, 105.0, 99.0, 103.0, 10.0)]);
        aggregator.insert(secondary, &[kline(1_000, 101.0, 107.0, 98.0, 106.0, 4.0)]);

        let merged = aggregator.composite_klines();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].open, Price::from_f64(100.0));
        assert_eq!(merged[0].close, Price::from_f64(103.0));
        assert_eq!(merged[0].high, Price::from_f64(107.0));
        assert_eq!(merged[0].low, Price::from_f64(98.0));
        assert_eq!(merged[0].volume.total(), Qty::from_f64(14.0));
    }
}
