use exchange::{
    Kline, PushFrequency, TickMultiplier, Ticker, TickerInfo, Timeframe, UnixMs, Volume,
    adapter::{Exchange, MarketKind, StreamKind, Venue},
    depth::Depth,
    unit::{Price, PriceStep, Qty},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DEPTH_SOURCE_STALE_AFTER_MS: u64 = 5_000;

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

    pub fn matches(self, ticker: Ticker) -> bool {
        self.ticker().same_market(&ticker)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedDefinition {
    pub id: AggregateFeedId,
    pub label: &'static str,
    pub sources: &'static [SourceDefinition],
    /// Shared price-grid precision used by every source combination.
    pub price_tick_power: i8,
}

const BTCUSDT_PERPETUAL_SOURCES: &[SourceDefinition] = &[
    SourceDefinition {
        exchange: Exchange::BinanceLinear,
        symbol: "BTCUSDT",
        label: "Binance · BTC/USDT",
    },
    SourceDefinition {
        exchange: Exchange::BybitLinear,
        symbol: "BTCUSDT",
        label: "Bybit · BTC/USDT",
    },
    SourceDefinition {
        exchange: Exchange::HyperliquidLinear,
        symbol: "BTC",
        label: "Hyperliquid · BTC/USDC",
    },
];

const BTCUSDT_PERPETUAL: FeedDefinition = FeedDefinition {
    id: AggregateFeedId::BtcUsdtPerpetual,
    label: "BTC Perpetual",
    sources: BTCUSDT_PERPETUAL_SOURCES,
    price_tick_power: -1,
};

/// Resolve equivalent linear perpetuals on Binance, Bybit, and Hyperliquid.
/// Venue symbols differ (`BTCUSDT` versus Hyperliquid's `BTC`), so matching is
/// based on the base asset instead of exact ticker text.
pub fn equivalent_footprint_sources(
    selected: TickerInfo,
    candidates: impl IntoIterator<Item = TickerInfo>,
) -> Vec<TickerInfo> {
    let Some(base) = canonical_base_asset(selected.ticker) else {
        return vec![selected];
    };

    let mut sources = candidates
        .into_iter()
        .filter(|candidate| {
            let (raw_symbol, _) = candidate.ticker.to_full_symbol_and_type();
            let is_builder_market =
                candidate.exchange().venue() == Venue::Hyperliquid && raw_symbol.contains(':');
            candidate.market_type() == MarketKind::LinearPerps
                && matches!(
                    candidate.exchange().venue(),
                    Venue::Binance | Venue::Bybit | Venue::Hyperliquid
                )
                // Builder-deployed Hyperliquid markets (for example
                // `hyna:BTC`) may use a different collateral and must not
                // silently replace Hyperliquid's native BTC perpetual.
                && (!is_builder_market || candidate.ticker.same_market(&selected.ticker))
                && canonical_base_asset(candidate.ticker).as_deref() == Some(base.as_str())
        })
        .collect::<Vec<_>>();

    if !sources
        .iter()
        .any(|source| source.ticker.same_market(&selected.ticker))
    {
        sources.push(selected);
    }

    sources.sort_by_key(|source| {
        let selected_rank = !source.ticker.same_market(&selected.ticker);
        let venue_rank = match source.exchange().venue() {
            Venue::Binance => 0,
            Venue::Bybit => 1,
            Venue::Hyperliquid => 2,
            Venue::Okex | Venue::Mexc => 3,
        };
        let symbol = source
            .ticker
            .to_full_symbol_and_type()
            .0
            .to_ascii_uppercase();
        let quote_rank = if symbol.ends_with("USDT") {
            0
        } else if symbol.ends_with("USDC") {
            1
        } else {
            2
        };
        (selected_rank, venue_rank, quote_rank)
    });
    let mut seen_venues = FxHashSet::default();
    sources.retain(|source| seen_venues.insert(source.exchange().venue()));
    sources
}

fn canonical_base_asset(ticker: Ticker) -> Option<String> {
    let (raw, market) = ticker.to_full_symbol_and_type();
    if market != MarketKind::LinearPerps {
        return None;
    }

    let symbol = raw
        .rsplit_once(':')
        .map_or(raw.as_str(), |(_, suffix)| suffix)
        .to_ascii_uppercase();
    let base = ["USDT", "USDC", "BUSD", "FDUSD", "USD"]
        .into_iter()
        .find_map(|quote| symbol.strip_suffix(quote))
        .unwrap_or(symbol.as_str());

    (!base.is_empty()).then(|| base.to_string())
}

impl AggregateFeedId {
    pub fn definition(self) -> &'static FeedDefinition {
        match self {
            Self::BtcUsdtPerpetual => &BTCUSDT_PERPETUAL,
        }
    }

    /// Stable price grid shared by every venue in this logical market.
    pub fn price_step(self) -> exchange::unit::PriceStep {
        exchange::unit::MinTicksize::new(self.definition().price_tick_power).into()
    }

    pub fn for_seed_ticker(ticker: Ticker) -> Option<Self> {
        [Self::BtcUsdtPerpetual].into_iter().find(|feed| {
            feed.definition()
                .sources
                .iter()
                .any(|source| source.matches(ticker))
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
            .find(|source| source.matches(ticker))
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
        for source in id.definition().sources {
            if let Some(info) = candidates
                .iter()
                .copied()
                .find(|candidate| source.matches(candidate.ticker))
                .or_else(|| source.matches(primary.ticker).then_some(primary))
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
            .filter(|source| {
                selected.is_none_or(|wanted| {
                    wanted
                        .iter()
                        .any(|ticker| ticker.same_market(&source.ticker))
                })
            })
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

    /// Stable base price tick for consumers that combine venue data.
    /// A logical feed keeps this grid even when only one source is enabled.
    pub fn price_step(&self) -> exchange::unit::PriceStep {
        self.id.map_or_else(
            || self.primary().min_ticksize.into(),
            AggregateFeedId::price_step,
        )
    }

    /// Returns the next non-empty source selection in catalog order.
    /// `None` means the requested toggle would disable the final source.
    pub fn toggled_source_tickers(&self, ticker: Ticker, enabled: bool) -> Option<Vec<Ticker>> {
        if !self
            .available_sources
            .iter()
            .any(|source| source.ticker.same_market(&ticker))
        {
            return None;
        }

        let selected = self
            .available_sources
            .iter()
            .filter_map(|source| {
                let is_selected = self.sources.contains(source);
                let keep = if source.ticker.same_market(&ticker) {
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

    pub fn depth_streams(&self) -> Vec<StreamKind> {
        self.sources
            .iter()
            .copied()
            .map(|ticker_info| StreamKind::Depth {
                ticker_info,
                depth_aggr: ticker_info
                    .exchange()
                    .stream_ticksize(Some(TickMultiplier(1)), TickMultiplier(1)),
                push_freq: PushFrequency::ServerDefault,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct SourceDepth {
    depth: Depth,
    update_time: UnixMs,
}

/// Source-aware current-book merger for a logical market.
///
/// Every adapter has already normalized quantity units. The merger rebins each
/// venue onto the shared price grid and sums every fresh book currently
/// available. This lets the heatmap render immediately while slower or
/// temporarily disconnected venues join the composite when they recover.
#[derive(Debug, Clone)]
pub struct DepthAggregator {
    sources: Vec<TickerInfo>,
    price_step: PriceStep,
    books: FxHashMap<TickerInfo, SourceDepth>,
}

impl DepthAggregator {
    pub fn new(sources: Vec<TickerInfo>, price_step: PriceStep) -> Self {
        Self {
            sources,
            price_step,
            books: FxHashMap::default(),
        }
    }

    pub fn sources(&self) -> &[TickerInfo] {
        &self.sources
    }

    pub fn insert(
        &mut self,
        source: TickerInfo,
        depth: &Depth,
        update_time: UnixMs,
    ) -> Option<Depth> {
        if !self.sources.contains(&source) {
            return None;
        }

        if self
            .books
            .get(&source)
            .is_some_and(|current| update_time < current.update_time)
        {
            return self.composite();
        }

        self.books.insert(
            source,
            SourceDepth {
                depth: depth.clone(),
                update_time,
            },
        );
        self.composite()
    }

    fn composite(&self) -> Option<Depth> {
        if self.sources.is_empty() || self.books.is_empty() {
            return None;
        }

        let latest_time = self
            .sources
            .iter()
            .filter_map(|source| self.books.get(source))
            .map(|book| book.update_time.as_u64())
            .max()?;

        let mut combined = Depth::default();
        for source in &self.sources {
            let Some(book) = self.books.get(source) else {
                continue;
            };
            if latest_time.saturating_sub(book.update_time.as_u64()) > DEPTH_SOURCE_STALE_AFTER_MS {
                continue;
            }

            merge_depth_side(&mut combined.bids, &book.depth.bids, self.price_step, true);
            merge_depth_side(&mut combined.asks, &book.depth.asks, self.price_step, false);
        }

        Some(combined)
    }
}

fn merge_depth_side(
    target: &mut BTreeMap<Price, Qty>,
    source: &BTreeMap<Price, Qty>,
    step: PriceStep,
    is_bid: bool,
) {
    for (price, qty) in source {
        let rounded = price.round_to_side_step(is_bid, step);
        *target.entry(rounded).or_default() += *qty;
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

    pub fn latest(&self, source: TickerInfo) -> Option<exchange::UnixMs> {
        self.bars
            .get(&source)
            .and_then(|bars| bars.last_key_value().map(|(time, _)| *time))
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

    fn depth(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> Depth {
        Depth {
            bids: bids
                .iter()
                .map(|(price, qty)| (Price::from_f64(*price), Qty::from_f64(*qty)))
                .collect(),
            asks: asks
                .iter()
                .map(|(price, qty)| (Price::from_f64(*price), Qty::from_f64(*qty)))
                .collect(),
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
        assert_eq!(
            AggregateFeedId::for_seed_ticker(Ticker::new_with_display(
                "BTC",
                Exchange::HyperliquidLinear,
                Some("BTCUSDC"),
            )),
            Some(AggregateFeedId::BtcUsdtPerpetual)
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
        let hyperliquid = TickerInfo::new(
            Ticker::new_with_display("BTC", Exchange::HyperliquidLinear, Some("BTCUSDC")),
            0.1,
            0.001,
            None,
        );

        let feed = ResolvedFeed::aggregated(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &[hyperliquid, binance, bybit],
        );

        assert_eq!(feed.sources(), &[binance, bybit, hyperliquid]);
        assert_eq!(feed.trade_streams().len(), 3);
        assert_eq!(feed.kline_streams(Timeframe::M1).len(), 3);
        assert_eq!(feed.depth_streams().len(), 3);
    }

    #[test]
    fn depth_aggregator_renders_available_sources_then_sums_rebinned_levels() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC");
        let step: PriceStep = exchange::unit::MinTicksize::new(0).into();
        let mut aggregator = DepthAggregator::new(vec![binance, bybit, hyperliquid], step);

        let binance_only = aggregator
            .insert(
                binance,
                &depth(&[(100.4, 1.0)], &[(101.0, 2.0)]),
                UnixMs::new(1_000),
            )
            .expect("the first available source should render immediately");
        assert_eq!(
            binance_only.bids.get(&Price::from_f64(100.0)),
            Some(&Qty::from_f64(1.0))
        );

        let two_sources = aggregator
            .insert(
                bybit,
                &depth(&[(100.9, 3.0)], &[(101.7, 4.0)]),
                UnixMs::new(1_001),
            )
            .expect("new sources should be added as they become ready");
        assert_eq!(
            two_sources.bids.get(&Price::from_f64(100.0)),
            Some(&Qty::from_f64(4.0))
        );
        let combined = aggregator
            .insert(
                hyperliquid,
                &depth(&[(100.2, 5.0)], &[(101.2, 6.0)]),
                UnixMs::new(1_002),
            )
            .expect("all three source books are ready");

        assert_eq!(
            combined.bids.get(&Price::from_f64(100.0)),
            Some(&Qty::from_f64(9.0))
        );
        assert_eq!(
            combined.asks.get(&Price::from_f64(102.0)),
            Some(&Qty::from_f64(10.0))
        );
        assert_eq!(
            combined.asks.get(&Price::from_f64(101.0)),
            Some(&Qty::from_f64(2.0))
        );
    }

    #[test]
    fn depth_aggregator_excludes_stale_books_after_initialization() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let step: PriceStep = exchange::unit::MinTicksize::new(0).into();
        let mut aggregator = DepthAggregator::new(vec![binance, bybit], step);
        let initial = depth(&[(100.0, 1.0)], &[(101.0, 1.0)]);

        assert!(
            aggregator
                .insert(binance, &initial, UnixMs::new(1_000))
                .is_some()
        );
        assert!(
            aggregator
                .insert(bybit, &initial, UnixMs::new(1_000))
                .is_some()
        );

        let fresh = depth(&[(100.0, 4.0)], &[(101.0, 4.0)]);
        let combined = aggregator
            .insert(binance, &fresh, UnixMs::new(7_001))
            .expect("initialized aggregate remains available");
        assert_eq!(
            combined.bids.get(&Price::from_f64(100.0)),
            Some(&Qty::from_f64(4.0))
        );
    }

    #[test]
    fn footprint_sources_resolve_selected_symbol_across_venue_symbol_formats() {
        let binance = ticker_info(Exchange::BinanceLinear, "SOLUSDT");
        let bybit = ticker_info(Exchange::BybitLinear, "SOLUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "SOL");
        let unrelated = ticker_info(Exchange::BinanceLinear, "BTCUSDT");

        let sources = equivalent_footprint_sources(bybit, [unrelated, hyperliquid, binance, bybit]);

        assert_eq!(sources, vec![bybit, binance, hyperliquid]);
    }

    #[test]
    fn footprint_sources_keep_only_one_market_per_venue() {
        let selected = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let binance_usdc = ticker_info(Exchange::BinanceLinear, "BTCUSDC");
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT");
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC");
        let hyperliquid_alias = TickerInfo::new(
            Ticker::new_with_display("BTC", Exchange::HyperliquidLinear, Some("BTC/USDC")),
            1.0,
            0.001,
            None,
        );

        let sources = equivalent_footprint_sources(
            selected,
            [
                selected,
                binance_usdc,
                bybit,
                hyperliquid,
                hyperliquid_alias,
            ],
        );
        assert_eq!(sources.len(), 3);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.exchange().venue())
                .collect::<Vec<_>>(),
            vec![Venue::Binance, Venue::Bybit, Venue::Hyperliquid]
        );
    }

    #[test]
    fn footprint_sources_exclude_hyperliquid_builder_market_aliases() {
        let selected = ticker_info(Exchange::BinanceLinear, "BTCUSDT");
        let native = TickerInfo::new(
            Ticker::new_with_display("BTC", Exchange::HyperliquidLinear, Some("BTCUSDC")),
            0.1,
            0.001,
            None,
        );
        let builder = TickerInfo::new(
            Ticker::new_with_display(
                "hyna:BTC",
                Exchange::HyperliquidLinear,
                Some("hyna:BTCUSDE"),
            ),
            0.1,
            0.001,
            None,
        );

        let sources = equivalent_footprint_sources(selected, [builder, native, selected]);

        assert!(
            sources
                .iter()
                .any(|source| source.ticker.same_market(&native.ticker))
        );
        assert!(
            !sources
                .iter()
                .any(|source| source.ticker.same_market(&builder.ticker))
        );
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

        let hyperliquid_only = ResolvedFeed::aggregated_selected(
            AggregateFeedId::BtcUsdtPerpetual,
            binance,
            &candidates,
            Some(&[hyperliquid.ticker]),
        );
        let shared_step: exchange::unit::PriceStep = exchange::unit::MinTicksize::new(-1).into();
        assert_eq!(bybit_only.price_step(), shared_step);
        assert_eq!(bybit_hyperliquid.price_step(), shared_step);
        assert_eq!(hyperliquid_only.price_step(), shared_step);
        assert_eq!(
            crate::chart::tpo::Config {
                ticks_per_row: 500,
                ..crate::chart::tpo::Config::default()
            }
            .row_step(hyperliquid_only.price_step())
            .to_ui_string(),
            "50"
        );
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
