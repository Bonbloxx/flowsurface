use crate::aggregation::AggregateFeedId;
use exchange::{PushFrequency, Ticker, TickerInfo, Timeframe, adapter::StreamKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum PersistStreamKind {
    Kline {
        ticker: Ticker,
        timeframe: Timeframe,
    },
    Depth(PersistDepth),
    Trades {
        ticker: Ticker,
    },
    /// Logical trade feed expanded through the aggregation catalog on load.
    AggregateTrades {
        feed: AggregateFeedId,
    },
    /// Deprecated combined stream, kept for backward compatibility.
    /// Will be converted to separate Depth and Trades on load.
    DepthAndTrades(PersistDepth),
}

impl PersistStreamKind {
    pub fn exchange(&self) -> exchange::adapter::Exchange {
        match self {
            PersistStreamKind::Kline { ticker, .. } => ticker.exchange,
            PersistStreamKind::Depth(d) => d.ticker.exchange,
            PersistStreamKind::Trades { ticker } => ticker.exchange,
            PersistStreamKind::AggregateTrades { feed } => {
                feed.definition()
                    .sources
                    .first()
                    .expect("aggregate feed has at least one source")
                    .exchange
            }
            PersistStreamKind::DepthAndTrades(d) => d.ticker.exchange,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PersistDepth {
    pub ticker: Ticker,
    #[serde(default = "default_depth_aggr")]
    pub depth_aggr: exchange::adapter::StreamTicksize,
    #[serde(default = "default_push_freq")]
    pub push_freq: PushFrequency,
}

impl From<StreamKind> for PersistStreamKind {
    fn from(stream: StreamKind) -> Self {
        match stream {
            StreamKind::Kline {
                ticker_info,
                timeframe,
            } => PersistStreamKind::Kline {
                ticker: ticker_info.ticker,
                timeframe,
            },
            StreamKind::Depth {
                ticker_info,
                depth_aggr,
                push_freq,
            } => PersistStreamKind::Depth(PersistDepth {
                ticker: ticker_info.ticker,
                depth_aggr,
                push_freq,
            }),
            StreamKind::Trades { ticker_info } => PersistStreamKind::Trades {
                ticker: ticker_info.ticker,
            },
        }
    }
}

impl PersistStreamKind {
    /// Try to convert into runtime StreamKind list. `resolver` should return Some(TickerInfo) for a ticker,
    /// otherwise the conversion fails (so caller can trigger a refresh / fetch).
    pub fn into_stream_kinds<F>(self, mut resolver: F) -> Result<Vec<StreamKind>, String>
    where
        F: FnMut(&Ticker) -> Option<TickerInfo>,
    {
        match self {
            PersistStreamKind::Kline { ticker, timeframe } => resolver(&ticker)
                .map(|ti| {
                    vec![StreamKind::Kline {
                        ticker_info: ti,
                        timeframe,
                    }]
                })
                .ok_or_else(|| format!("Ticker metadata not found for {}", ticker)),
            PersistStreamKind::Depth(d) => resolver(&d.ticker)
                .map(|ti| {
                    vec![StreamKind::Depth {
                        ticker_info: ti,
                        depth_aggr: d.depth_aggr,
                        push_freq: d.push_freq,
                    }]
                })
                .ok_or_else(|| format!("Ticker metadata not found for {}", d.ticker)),
            PersistStreamKind::Trades { ticker } => resolver(&ticker)
                .map(|ti| vec![StreamKind::Trades { ticker_info: ti }])
                .ok_or_else(|| format!("Ticker metadata not found for {}", ticker)),
            PersistStreamKind::AggregateTrades { feed } => feed
                .source_tickers()
                .map(|ticker| {
                    resolver(&ticker)
                        .map(|ticker_info| StreamKind::Trades { ticker_info })
                        .ok_or_else(|| format!("Ticker metadata not found for {}", ticker))
                })
                .collect(),
            PersistStreamKind::DepthAndTrades(d) => resolver(&d.ticker)
                .map(|ti| {
                    vec![
                        StreamKind::Depth {
                            ticker_info: ti,
                            depth_aggr: d.depth_aggr,
                            push_freq: d.push_freq,
                        },
                        StreamKind::Trades { ticker_info: ti },
                    ]
                })
                .ok_or_else(|| format!("Ticker metadata not found for {}", d.ticker)),
        }
    }
}

fn default_depth_aggr() -> exchange::adapter::StreamTicksize {
    exchange::adapter::StreamTicksize::Client
}

fn default_push_freq() -> PushFrequency {
    PushFrequency::ServerDefault
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregation::AggregateFeedId;
    use exchange::adapter::Exchange;

    #[test]
    fn aggregate_trade_stream_resolves_catalog_sources() {
        let binance = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let bybit = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BybitLinear),
            0.1,
            0.001,
            None,
        );
        let hyperliquid = TickerInfo::new(
            Ticker::new("BTC", Exchange::HyperliquidLinear),
            1.0,
            0.00001,
            None,
        );
        let persisted = PersistStreamKind::AggregateTrades {
            feed: AggregateFeedId::BtcUsdtPerpetual,
        };

        let streams = persisted
            .into_stream_kinds(|ticker| {
                [binance, bybit, hyperliquid]
                    .into_iter()
                    .find(|info| info.ticker == *ticker)
            })
            .expect("aggregate feed resolves");

        assert_eq!(
            streams,
            vec![
                StreamKind::Trades {
                    ticker_info: binance
                },
                StreamKind::Trades { ticker_info: bybit },
                StreamKind::Trades {
                    ticker_info: hyperliquid
                }
            ]
        );
    }
}
