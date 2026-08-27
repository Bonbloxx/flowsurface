use crate::{
    Event, Kline, Price, PushFrequency, TickMultiplier, Ticker, TickerInfo, Timeframe, Trade,
    Volume,
    adapter::{
        MarketKind, StreamKind, StreamTicksize,
        hub::{TradeBuffer, WsAdapter, WsSession, WsTransport},
    },
    depth::{DeOrder, DepthPayload, DepthUpdate, LocalDepthCache},
    serde_util::de_string_to_number,
    unit::qty::{QtyNormalization, SizeUnit, volume_size_unit},
};

use super::{HyperliquidHandle, WS_DOMAIN, raw_qty_unit_from_market_type};
use crate::adapter::hub::AdapterError;
use fastwebsockets::Frame;
use futures::Stream;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};

const SIG_FIG_LIMIT: i32 = 5;
const ALLOWED_MANTISSA: [i32; 3] = [1, 2, 5];
const HYPERLIQUID_PING_PAYLOAD: &[u8] = br#"{"method":"ping"}"#;
/// Hyperliquid sends a 30-trade reconnect snapshot. Keep a much wider rolling
/// identity window so every replayed execution remains recognizable.
const RECENT_TRADE_ID_CAPACITY: usize = 4_096;

#[derive(Clone, Copy, Debug)]
struct DepthFeedConfig {
    pub n_sig_figs: Option<i32>,
    pub mantissa: Option<i32>,
}

impl DepthFeedConfig {
    fn new(n_sig_figs: Option<i32>, mantissa: Option<i32>) -> Self {
        Self {
            n_sig_figs,
            mantissa,
        }
    }

    fn full_precision() -> Self {
        Self {
            n_sig_figs: None,
            mantissa: None,
        }
    }
}

fn snap_multiplier_to_125(multiplier: u16) -> (i32, i32) {
    const SQRT2: f32 = std::f32::consts::SQRT_2;
    const SQRT10: f32 = 3.162_277_7;
    const SQRT50: f32 = 7.071_068;

    let m = (multiplier as f32).max(1.0);
    let mut kf = m.log10().floor();
    let rem = m / 10_f32.powf(kf);

    let (mantissa, bump) = if rem < SQRT2 {
        (1, false)
    } else if rem < SQRT10 {
        (2, false)
    } else if rem < SQRT50 {
        (5, false)
    } else {
        (1, true)
    };

    if bump {
        kf += 1.0;
    }

    (kf as i32, mantissa)
}

fn config_from_multiplier(price: f64, multiplier: u16) -> DepthFeedConfig {
    if price <= 0.0 {
        return DepthFeedConfig::full_precision();
    }
    if multiplier <= 1 {
        return DepthFeedConfig::full_precision();
    }

    let int_digits = if price >= 1.0 {
        (price.abs().log10().floor() as i32 + 1).max(1)
    } else {
        0
    };

    let (k, m125) = snap_multiplier_to_125(multiplier);
    let n = if int_digits > SIG_FIG_LIMIT {
        (int_digits - k).clamp(2, SIG_FIG_LIMIT)
    } else {
        (SIG_FIG_LIMIT - k).clamp(2, SIG_FIG_LIMIT)
    };

    let mantissa = if n == SIG_FIG_LIMIT && (m125 == 2 || m125 == 5) {
        Some(m125)
    } else {
        None
    };

    DepthFeedConfig::new(Some(n), mantissa)
}

#[derive(Debug, Deserialize)]
struct HyperliquidDepth {
    levels: [Vec<HyperliquidLevel>; 2],
    time: u64,
}

#[derive(Debug, Deserialize)]
struct HyperliquidLevel {
    #[serde(deserialize_with = "de_string_to_number")]
    px: f64,
    #[serde(deserialize_with = "de_string_to_number")]
    sz: f64,
}

#[derive(Debug, Deserialize)]
struct HyperliquidTrade {
    coin: String,
    side: String,
    #[serde(deserialize_with = "de_string_to_number")]
    px: f64,
    #[serde(deserialize_with = "de_string_to_number")]
    sz: f64,
    time: u64,
    tid: u64,
}

/// Hyperliquid documents `(block_time, coin, tid)` as a globally unique trade
/// identity. This value is already partitioned by coin, so `(time, tid)` is the
/// exact key retained here.
#[derive(Default)]
struct RecentTradeIds {
    order: VecDeque<(u64, u64)>,
    ids: FxHashSet<(u64, u64)>,
}

impl RecentTradeIds {
    /// Returns `true` only for a newly observed execution.
    fn insert(&mut self, time: u64, tid: u64) -> bool {
        let key = (time, tid);
        if !self.ids.insert(key) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > RECENT_TRADE_ID_CAPACITY {
            if let Some(expired) = self.order.pop_front() {
                self.ids.remove(&expired);
            }
        }
        true
    }

    fn contains(&self, time: u64, tid: u64) -> bool {
        self.ids.contains(&(time, tid))
    }
}

#[derive(Debug, Deserialize)]
struct HyperliquidKline {
    #[serde(rename = "t")]
    time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "i")]
    interval: String,
    #[serde(rename = "o", deserialize_with = "de_string_to_number")]
    open: f64,
    #[serde(rename = "h", deserialize_with = "de_string_to_number")]
    high: f64,
    #[serde(rename = "l", deserialize_with = "de_string_to_number")]
    low: f64,
    #[serde(rename = "c", deserialize_with = "de_string_to_number")]
    close: f64,
    #[serde(rename = "v", deserialize_with = "de_string_to_number")]
    volume: f64,
}

enum StreamData {
    Trade(Vec<HyperliquidTrade>),
    Depth(HyperliquidDepth),
    Kline(HyperliquidKline),
}

async fn connect_websocket(
    domain: &str,
    path: &str,
    proxy_cfg: Option<&crate::proxy::Proxy>,
) -> Result<WsTransport, AdapterError> {
    let url = format!("wss://{}{}", domain, path);
    WsTransport::establish(domain, &url, proxy_cfg).await
}

fn parse_websocket_message(payload: &[u8]) -> Result<StreamData, AdapterError> {
    let json: Value =
        serde_json::from_slice(payload).map_err(|e| AdapterError::ParseError(e.to_string()))?;

    let channel = json
        .get("channel")
        .and_then(|c| c.as_str())
        .ok_or_else(|| AdapterError::ParseError("Missing channel".to_string()))?;

    match channel {
        "trades" => {
            let trades: Vec<HyperliquidTrade> = serde_json::from_value(json["data"].clone())
                .map_err(|e| AdapterError::ParseError(e.to_string()))?;
            Ok(StreamData::Trade(trades))
        }
        "l2Book" => {
            let depth: HyperliquidDepth = serde_json::from_value(json["data"].clone())
                .map_err(|e| AdapterError::ParseError(e.to_string()))?;
            Ok(StreamData::Depth(depth))
        }
        "candle" => {
            let kline: HyperliquidKline = serde_json::from_value(json["data"].clone())
                .map_err(|e| AdapterError::ParseError(e.to_string()))?;
            Ok(StreamData::Kline(kline))
        }
        _ => Err(AdapterError::ParseError(format!(
            "Unknown channel: {}",
            channel
        ))),
    }
}

struct TradeAdapter {
    symbol_to_ticker: FxHashMap<String, Ticker>,
    buffer: TradeBuffer,
    subscription_coins: Vec<String>,
    stream_by_coin: FxHashMap<String, StreamKind>,
    recent_ids: FxHashMap<String, RecentTradeIds>,
    snapshot_pending: FxHashSet<String>,
    proxy_cfg: Option<crate::proxy::Proxy>,
}

impl WsAdapter for TradeAdapter {
    async fn connect(&mut self) -> Result<WsTransport, String> {
        let mut websocket = connect_websocket(WS_DOMAIN, "/ws", self.proxy_cfg.as_ref())
            .await
            .map_err(|e| format!("Failed to connect to websocket: {e}"))?;

        for symbol_str in &self.subscription_coins {
            let trades_subscribe_msg = json!({
                "method": "subscribe",
                "subscription": {
                    "type": "trades",
                    "coin": symbol_str
                }
            });

            websocket
                .write_frame(Frame::text(fastwebsockets::Payload::Borrowed(
                    trades_subscribe_msg.to_string().as_bytes(),
                )))
                .await
                .map_err(|e| format!("Failed subscribing: {e}"))?;
        }

        Ok(websocket)
    }

    async fn on_connected(&mut self) -> Vec<Event> {
        self.snapshot_pending = self.subscription_coins.iter().cloned().collect();
        self.buffer.flush()
    }

    async fn on_text(&mut self, payload: &[u8]) -> Result<Vec<Event>, String> {
        if let Ok(StreamData::Trade(trades)) = parse_websocket_message(payload) {
            let mut replay_from = FxHashMap::<String, u64>::default();
            let snapshot_coins = trades
                .iter()
                .map(|trade| trade.coin.clone())
                .collect::<FxHashSet<_>>();

            // Hyperliquid sends recent executions immediately after every
            // subscription. First classify the whole payload against IDs from
            // before this connection. Only an overlap proves this is a
            // reconnect replay that bridges the previous live interval.
            for hl_trade in &trades {
                if self.snapshot_pending.contains(&hl_trade.coin)
                    && self
                        .recent_ids
                        .get(&hl_trade.coin)
                        .is_some_and(|recent| recent.contains(hl_trade.time, hl_trade.tid))
                {
                    replay_from
                        .entry(hl_trade.coin.clone())
                        .and_modify(|from| *from = (*from).min(hl_trade.time))
                        .or_insert(hl_trade.time);
                }
            }

            for hl_trade in trades {
                let is_snapshot = self.snapshot_pending.contains(&hl_trade.coin);
                let snapshot_has_overlap = replay_from.contains_key(&hl_trade.coin);
                let recent = self.recent_ids.entry(hl_trade.coin.clone()).or_default();
                if !recent.insert(hl_trade.time, hl_trade.tid) {
                    continue;
                }

                // With no overlap, this is either the initial subscription or
                // an unproven reconnect after a long outage/process restart.
                // Seed its IDs for later deduplication, but never publish those
                // pre-connection rows as fresh live executions. If overlap is
                // proven, publish only previously unseen rows: those are the
                // executions recovered by the replay.
                if is_snapshot && !snapshot_has_overlap {
                    continue;
                }
                if let Some(ticker) = self.symbol_to_ticker.get(&hl_trade.coin)
                    && let Some((ticker_info, qty_norm)) = self.buffer.ticker_info(ticker)
                {
                    let ticker_info = *ticker_info;
                    let qty_norm = *qty_norm;
                    let price =
                        Price::from_f64(hl_trade.px).round_to_min_tick(ticker_info.min_ticksize);
                    self.buffer.push(
                        *ticker,
                        Trade {
                            time: hl_trade.time.into(),
                            is_sell: hl_trade.side == "A",
                            price,
                            qty: qty_norm.normalize_qty(hl_trade.sz, hl_trade.px),
                        },
                    );
                } else {
                    log::error!(
                        "Ticker info not found for Hyperliquid coin: {}",
                        hl_trade.coin
                    );
                }
            }

            for coin in snapshot_coins {
                self.snapshot_pending.remove(&coin);
            }

            let mut events = Vec::with_capacity(replay_from.len());
            for (coin, from) in replay_from {
                if let Some(stream) = self.stream_by_coin.get(&coin) {
                    events.push(Event::ReplayRecovered(Arc::from([*stream]), from.into()));
                }
            }
            return Ok(events);
        }

        Ok(Vec::new())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Vec<Event> {
        self.buffer.flush()
    }

    async fn on_tick(&mut self) -> Vec<Event> {
        self.buffer.flush()
    }
}

pub fn connect_trade_stream(
    tickers: Vec<TickerInfo>,
    market_type: MarketKind,
    proxy_cfg: Option<crate::proxy::Proxy>,
) -> impl Stream<Item = Event> {
    let stream_scope: Arc<[StreamKind]> = Arc::from(
        tickers
            .iter()
            .map(|ticker_info| StreamKind::Trades {
                ticker_info: *ticker_info,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );

    let ticker_info_map = tickers
        .iter()
        .map(|ticker_info| {
            (
                ticker_info.ticker,
                (
                    *ticker_info,
                    QtyNormalization::with_raw_qty_unit(
                        volume_size_unit() == SizeUnit::Quote,
                        *ticker_info,
                        raw_qty_unit_from_market_type(market_type),
                    ),
                ),
            )
        })
        .collect();

    let symbol_to_ticker = tickers
        .iter()
        .map(|ticker_info| {
            let (symbol_str, _) = ticker_info.ticker.to_full_symbol_and_type();
            (symbol_str, ticker_info.ticker)
        })
        .collect();

    let subscription_coins = tickers
        .iter()
        .map(|ticker_info| ticker_info.ticker.to_full_symbol_and_type().0)
        .collect();

    let stream_by_coin = tickers
        .iter()
        .map(|ticker_info| {
            (
                ticker_info.ticker.to_full_symbol_and_type().0,
                StreamKind::Trades {
                    ticker_info: *ticker_info,
                },
            )
        })
        .collect();

    let adapter = TradeAdapter {
        symbol_to_ticker,
        buffer: TradeBuffer::new(ticker_info_map),
        subscription_coins,
        stream_by_coin,
        recent_ids: FxHashMap::default(),
        snapshot_pending: FxHashSet::default(),
        proxy_cfg,
    };

    WsSession::with_text_ping(HYPERLIQUID_PING_PAYLOAD, stream_scope).run(adapter)
}

struct DepthAdapter {
    handle: HyperliquidHandle,
    stream: StreamKind,
    ticker_info: TickerInfo,
    symbol_str: String,
    qty_norm: QtyNormalization,
    user_multiplier: u16,
    local_depth_cache: LocalDepthCache,
    pending_snapshot_emit_ms: Option<u64>,
    proxy_cfg: Option<crate::proxy::Proxy>,
}

impl WsAdapter for DepthAdapter {
    async fn connect(&mut self) -> Result<WsTransport, String> {
        let snapshot = self
            .handle
            .fetch_depth_snapshot(self.ticker_info.ticker)
            .await
            .map_err(|e| format!("Failed to fetch depth snapshot: {e}"))?;

        let Some(best_bid_price) = snapshot.bids.first().map(|o| o.price) else {
            return Err("Depth snapshot missing bids".to_string());
        };

        let depth_cfg = config_from_multiplier(best_bid_price, self.user_multiplier);
        let snapshot_time_ms = snapshot.time.as_u64();

        self.local_depth_cache.update_with_qty_norm(
            DepthUpdate::Snapshot(snapshot),
            self.ticker_info.min_ticksize,
            Some(self.qty_norm),
        );
        self.pending_snapshot_emit_ms = Some(snapshot_time_ms);

        let mut websocket = connect_websocket(WS_DOMAIN, "/ws", self.proxy_cfg.as_ref())
            .await
            .map_err(|e| format!("Failed to connect to websocket: {e}"))?;

        let mut depth_subscription = json!({
            "method": "subscribe",
            "subscription": {
                "type": "l2Book",
                "coin": self.symbol_str,
            }
        });

        if let Some(n) = depth_cfg.n_sig_figs {
            depth_subscription["subscription"]["nSigFigs"] = json!(n);
        }
        if let (Some(m), Some(5)) = (depth_cfg.mantissa, depth_cfg.n_sig_figs)
            && m != 1
            && ALLOWED_MANTISSA.contains(&m)
        {
            depth_subscription["subscription"]["mantissa"] = json!(m);
        }

        websocket
            .write_frame(Frame::text(fastwebsockets::Payload::Borrowed(
                depth_subscription.to_string().as_bytes(),
            )))
            .await
            .map_err(|e| format!("Failed subscribing: {e}"))?;

        Ok(websocket)
    }

    async fn on_connected(&mut self) -> Vec<Event> {
        if let Some(snapshot_time_ms) = self.pending_snapshot_emit_ms.take() {
            vec![Event::DepthReceived(
                self.stream,
                snapshot_time_ms.into(),
                self.local_depth_cache.depth.clone(),
            )]
        } else {
            Vec::new()
        }
    }

    async fn on_text(&mut self, payload: &[u8]) -> Result<Vec<Event>, String> {
        if let Ok(StreamData::Depth(depth)) = parse_websocket_message(payload) {
            let bids = depth.levels[0]
                .iter()
                .map(|level| DeOrder {
                    price: level.px,
                    qty: level.sz,
                })
                .collect();
            let asks = depth.levels[1]
                .iter()
                .map(|level| DeOrder {
                    price: level.px,
                    qty: level.sz,
                })
                .collect();

            let depth_payload = DepthPayload {
                last_update_id: depth.time,
                time: depth.time.into(),
                bids,
                asks,
            };
            self.local_depth_cache.update_with_qty_norm(
                DepthUpdate::Snapshot(depth_payload),
                self.ticker_info.min_ticksize,
                Some(self.qty_norm),
            );

            return Ok(vec![Event::DepthReceived(
                self.stream,
                depth.time.into(),
                self.local_depth_cache.depth.clone(),
            )]);
        }

        Ok(Vec::new())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Vec<Event> {
        Vec::new()
    }
}

pub fn connect_depth_stream(
    handle: HyperliquidHandle,
    ticker_info: TickerInfo,
    depth_aggr: StreamTicksize,
    push_freq: PushFrequency,
    proxy_cfg: Option<crate::proxy::Proxy>,
) -> impl Stream<Item = Event> {
    let tick_multiplier = match depth_aggr {
        StreamTicksize::ServerSide(multiplier) => Some(multiplier),
        StreamTicksize::Client => None,
    };
    let stream = StreamKind::Depth {
        ticker_info,
        depth_aggr,
        push_freq,
    };

    let stream_scope: Arc<[StreamKind]> = Arc::from(vec![stream].into_boxed_slice());

    let ticker = ticker_info.ticker;
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        volume_size_unit() == SizeUnit::Quote,
        ticker_info,
        raw_qty_unit_from_market_type(ticker_info.market_type()),
    );
    let user_multiplier = tick_multiplier.unwrap_or(TickMultiplier(1)).0;

    let (symbol_str, _) = ticker.to_full_symbol_and_type();

    let adapter = DepthAdapter {
        handle,
        stream,
        ticker_info,
        symbol_str,
        qty_norm,
        user_multiplier,
        local_depth_cache: LocalDepthCache::default(),
        pending_snapshot_emit_ms: None,
        proxy_cfg,
    };

    WsSession::with_text_ping(HYPERLIQUID_PING_PAYLOAD, stream_scope).run(adapter)
}

struct KlineAdapter {
    market_type: MarketKind,
    size_in_quote_ccy: bool,
    stream_lookup: FxHashMap<(String, String), (TickerInfo, Timeframe)>,
    subscriptions: Vec<(String, String)>,
    proxy_cfg: Option<crate::proxy::Proxy>,
}

impl WsAdapter for KlineAdapter {
    async fn connect(&mut self) -> Result<WsTransport, String> {
        let mut websocket = connect_websocket(WS_DOMAIN, "/ws", self.proxy_cfg.as_ref())
            .await
            .map_err(|e| format!("Failed to connect to websocket: {e}"))?;

        for (symbol_str, interval) in &self.subscriptions {
            let subscribe_msg = json!({
                "method": "subscribe",
                "subscription": {
                    "type": "candle",
                    "coin": symbol_str,
                    "interval": interval
                }
            });

            websocket
                .write_frame(Frame::text(fastwebsockets::Payload::Borrowed(
                    subscribe_msg.to_string().as_bytes(),
                )))
                .await
                .map_err(|e| format!("Failed subscribing: {e}"))?;
        }

        Ok(websocket)
    }

    async fn on_connected(&mut self) -> Vec<Event> {
        Vec::new()
    }

    async fn on_text(&mut self, payload: &[u8]) -> Result<Vec<Event>, String> {
        if let Ok(StreamData::Kline(hl_kline)) = parse_websocket_message(payload)
            && let Some((ticker_info, timeframe)) = self
                .stream_lookup
                .get(&(hl_kline.symbol.clone(), hl_kline.interval.clone()))
        {
            let qty_norm = QtyNormalization::with_raw_qty_unit(
                self.size_in_quote_ccy,
                *ticker_info,
                raw_qty_unit_from_market_type(self.market_type),
            );
            let volume = qty_norm.normalize_qty(hl_kline.volume, hl_kline.close);

            let kline = Kline::new(
                hl_kline.time,
                hl_kline.open,
                hl_kline.high,
                hl_kline.low,
                hl_kline.close,
                Volume::TotalOnly(volume),
                ticker_info.min_ticksize,
            );

            let stream_kind = StreamKind::Kline {
                ticker_info: *ticker_info,
                timeframe: *timeframe,
            };
            return Ok(vec![Event::KlineReceived(stream_kind, kline)]);
        }

        Ok(Vec::new())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Vec<Event> {
        Vec::new()
    }
}

pub fn connect_kline_stream(
    streams: Vec<(TickerInfo, Timeframe)>,
    market_type: MarketKind,
    proxy_cfg: Option<crate::proxy::Proxy>,
) -> impl Stream<Item = Event> {
    let stream_scope: Arc<[StreamKind]> = Arc::from(
        streams
            .iter()
            .map(|(ticker_info, timeframe)| StreamKind::Kline {
                ticker_info: *ticker_info,
                timeframe: *timeframe,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );

    let stream_lookup = streams
        .iter()
        .map(|(ticker_info, timeframe)| {
            (
                (
                    ticker_info.ticker.to_full_symbol_and_type().0,
                    timeframe.to_string(),
                ),
                (*ticker_info, *timeframe),
            )
        })
        .collect();

    let subscriptions = streams
        .iter()
        .map(|(ticker_info, timeframe)| {
            (
                ticker_info.ticker.to_full_symbol_and_type().0,
                timeframe.to_string(),
            )
        })
        .collect();

    let adapter = KlineAdapter {
        market_type,
        size_in_quote_ccy: volume_size_unit() == SizeUnit::Quote,
        stream_lookup,
        subscriptions,
        proxy_cfg,
    };

    WsSession::with_text_ping(HYPERLIQUID_PING_PAYLOAD, stream_scope).run(adapter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Exchange;

    fn test_trade_adapter() -> TradeAdapter {
        let ticker_info = TickerInfo::new(
            Ticker::new("BTC", Exchange::HyperliquidLinear),
            1.0,
            0.00001,
            None,
        );
        let ticker = ticker_info.ticker;
        let coin = "BTC".to_string();
        TradeAdapter {
            symbol_to_ticker: FxHashMap::from_iter([(coin.clone(), ticker)]),
            buffer: TradeBuffer::new(FxHashMap::from_iter([(
                ticker,
                (
                    ticker_info,
                    QtyNormalization::with_raw_qty_unit(
                        volume_size_unit() == SizeUnit::Quote,
                        ticker_info,
                        raw_qty_unit_from_market_type(MarketKind::LinearPerps),
                    ),
                ),
            )])),
            subscription_coins: vec![coin.clone()],
            stream_by_coin: FxHashMap::from_iter([(coin, StreamKind::Trades { ticker_info })]),
            recent_ids: FxHashMap::default(),
            snapshot_pending: FxHashSet::default(),
            proxy_cfg: None,
        }
    }

    fn trade_payload(trades: &str) -> Vec<u8> {
        format!(r#"{{"channel":"trades","data":[{trades}]}}"#).into_bytes()
    }

    fn trade_count(events: &[Event]) -> usize {
        events
            .iter()
            .map(|event| match event {
                Event::TradesReceived(_, _, trades) => trades.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn recent_trade_ids_reject_replayed_execution_identity() {
        let mut ids = RecentTradeIds::default();

        assert!(ids.insert(1_000, 42));
        assert!(!ids.insert(1_000, 42));
        assert!(ids.insert(1_000, 43));
        assert!(ids.insert(1_001, 42));
    }

    #[test]
    fn recent_trade_ids_evict_only_after_bounded_window() {
        let mut ids = RecentTradeIds::default();
        for id in 0..=RECENT_TRADE_ID_CAPACITY as u64 {
            assert!(ids.insert(id, id));
        }

        assert!(!ids.contains(0, 0));
        assert!(ids.contains(1, 1));
        assert!(ids.contains(
            RECENT_TRADE_ID_CAPACITY as u64,
            RECENT_TRADE_ID_CAPACITY as u64
        ));
    }

    #[test]
    fn trade_payload_retains_hyperliquid_tid() {
        let payload = br#"{
            "channel":"trades",
            "data":[{
                "coin":"BTC",
                "side":"B",
                "px":"78000",
                "sz":"0.01",
                "time":1234,
                "tid":5678
            }]
        }"#;

        let StreamData::Trade(trades) = parse_websocket_message(payload).expect("trade payload")
        else {
            panic!("expected trade payload");
        };
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].tid, 5_678);
    }

    #[tokio::test]
    async fn subscription_snapshots_are_fail_closed_and_reconnect_replay_is_deduplicated() {
        let mut adapter = test_trade_adapter();
        adapter.on_connected().await;
        let first = trade_payload(
            r#"{"coin":"BTC","side":"B","px":"78000","sz":"0.01","time":1000,"tid":1},
               {"coin":"BTC","side":"A","px":"78001","sz":"0.02","time":1001,"tid":2}"#,
        );
        assert!(
            adapter
                .on_text(&first)
                .await
                .expect("first snapshot")
                .is_empty()
        );
        assert_eq!(trade_count(&adapter.on_tick().await), 0);

        let live = trade_payload(
            r#"{"coin":"BTC","side":"B","px":"78002","sz":"0.03","time":1002,"tid":3}"#,
        );
        assert!(adapter.on_text(&live).await.expect("live trade").is_empty());
        assert_eq!(trade_count(&adapter.on_tick().await), 1);

        // Duplicate identities are rejected even outside the reconnect
        // snapshot boundary.
        assert!(
            adapter
                .on_text(&live)
                .await
                .expect("duplicate live trade")
                .is_empty()
        );
        assert_eq!(trade_count(&adapter.on_tick().await), 0);

        adapter.on_connected().await;
        let reconnect = trade_payload(
            r#"{"coin":"BTC","side":"B","px":"78002","sz":"0.03","time":1002,"tid":3},
               {"coin":"BTC","side":"A","px":"78003","sz":"0.04","time":1003,"tid":4}"#,
        );
        let recovery = adapter
            .on_text(&reconnect)
            .await
            .expect("reconnect snapshot");
        assert!(matches!(
            recovery.as_slice(),
            [Event::ReplayRecovered(streams, from)]
                if streams.len() == 1 && from.as_u64() == 1_002
        ));
        assert_eq!(trade_count(&adapter.on_tick().await), 1);
    }
}
