use crate::{
    Kline, OpenInterest, Price, Qty, Ticker, TickerInfo, TickerStats, Timeframe, Trade, UnixMs,
    Volume,
    depth::{DeOrder, DepthPayload},
    serde_util,
    serde_util::de_string_to_number,
    unit::qty::{QtyNormalization, SizeUnit, volume_size_unit},
};

use super::{
    BinanceLimiter, HttpHub, INVERSE_PERP_DOMAIN, LINEAR_PERP_DOMAIN, MarketKind, SPOT_DOMAIN,
    THIRTY_DAYS_MS, exchange_from_market_type, raw_qty_unit_from_market_type,
};
use crate::adapter::hub::AdapterError;
use csv::ReaderBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    io::BufReader,
    path::PathBuf,
    time::UNIX_EPOCH,
};

#[derive(Deserialize, Debug, Clone)]
struct FetchedKline(
    u64,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    u64,
    String,
    u32,
    #[serde(deserialize_with = "de_string_to_number")] f64,
    String,
    String,
);

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeOpenInterest {
    #[serde(rename = "timestamp")]
    pub time: u64,
    #[serde(rename = "sumOpenInterest", deserialize_with = "de_string_to_number")]
    pub sum: f64,
    #[serde(rename = "sumOpenInterestValue", default)]
    pub notional: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeCurrentOpenInterest {
    #[serde(deserialize_with = "de_string_to_number")]
    open_interest: f64,
    time: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DePremiumIndex {
    #[serde(deserialize_with = "de_string_to_number")]
    mark_price: f64,
}

#[derive(Deserialize, Debug)]
struct DeTrade {
    #[serde(rename = "a", default)]
    id: u64,
    #[serde(rename = "T")]
    time: u64,
    #[serde(rename = "p", deserialize_with = "de_string_to_number")]
    price: f64,
    #[serde(rename = "q", deserialize_with = "de_string_to_number")]
    qty: f64,
    #[serde(rename = "m")]
    is_sell: bool,
}

#[derive(Deserialize, Clone)]
struct FetchedPerpDepth {
    #[serde(rename = "lastUpdateId")]
    update_id: u64,
    #[serde(rename = "T")]
    time: u64,
    #[serde(rename = "bids")]
    bids: Vec<DeOrder>,
    #[serde(rename = "asks")]
    asks: Vec<DeOrder>,
}

#[derive(Deserialize, Clone)]
struct FetchedSpotDepth {
    #[serde(rename = "lastUpdateId")]
    update_id: u64,
    #[serde(rename = "bids")]
    bids: Vec<DeOrder>,
    #[serde(rename = "asks")]
    asks: Vec<DeOrder>,
}

pub(super) async fn fetch_depth_snapshot(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker: Ticker,
) -> Result<DepthPayload, AdapterError> {
    let (symbol_str, market_type) = ticker.to_full_symbol_and_type();

    let base_url = match market_type {
        MarketKind::Spot => format!("{SPOT_DOMAIN}/api/v3/depth"),
        MarketKind::LinearPerps => format!("{LINEAR_PERP_DOMAIN}/fapi/v1/depth"),
        MarketKind::InversePerps => format!("{INVERSE_PERP_DOMAIN}/dapi/v1/depth"),
    };

    let depth_limit = match market_type {
        MarketKind::Spot => 5000,
        MarketKind::LinearPerps | MarketKind::InversePerps => 1000,
    };

    let url = format!(
        "{}?symbol={}&limit={}",
        base_url,
        symbol_str.to_uppercase(),
        depth_limit
    );

    let weight = match market_type {
        MarketKind::Spot => match depth_limit {
            ..=100_i32 => 5,
            101_i32..=500_i32 => 25,
            501_i32..=1000_i32 => 50,
            1001_i32..=5000_i32 => 250,
            _ => {
                return Err(AdapterError::InvalidRequest(format!(
                    "Unsupported depth limit for spot market: {depth_limit}"
                )));
            }
        },
        MarketKind::LinearPerps | MarketKind::InversePerps => match depth_limit {
            ..100 => 2,
            100 => 5,
            500 => 10,
            1000 => 20,
            _ => {
                return Err(AdapterError::InvalidRequest(format!(
                    "Unsupported depth limit for perps market: {depth_limit}"
                )));
            }
        },
    };

    let text = hub.http_text_with_limiter(&url, weight, None, None).await?;

    match market_type {
        MarketKind::Spot => {
            let fetched_depth: FetchedSpotDepth =
                serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))?;

            Ok(DepthPayload {
                last_update_id: fetched_depth.update_id,
                time: (chrono::Utc::now().timestamp_millis() as u64).into(),
                bids: fetched_depth
                    .bids
                    .iter()
                    .map(|x| DeOrder {
                        price: x.price,
                        qty: x.qty,
                    })
                    .collect(),
                asks: fetched_depth
                    .asks
                    .iter()
                    .map(|x| DeOrder {
                        price: x.price,
                        qty: x.qty,
                    })
                    .collect(),
            })
        }
        MarketKind::LinearPerps | MarketKind::InversePerps => {
            let fetched_depth: FetchedPerpDepth =
                serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))?;

            Ok(DepthPayload {
                last_update_id: fetched_depth.update_id,
                time: fetched_depth.time.into(),
                bids: fetched_depth
                    .bids
                    .iter()
                    .map(|x| DeOrder {
                        price: x.price,
                        qty: x.qty,
                    })
                    .collect(),
                asks: fetched_depth
                    .asks
                    .iter()
                    .map(|x| DeOrder {
                        price: x.price,
                        qty: x.qty,
                    })
                    .collect(),
            })
        }
    }
}

pub(super) async fn fetch_ticker_metadata(
    hub: &mut HttpHub<BinanceLimiter>,
    market: MarketKind,
) -> Result<super::super::TickerMetadataMap, AdapterError> {
    let (url, weight) = match market {
        MarketKind::Spot => (format!("{SPOT_DOMAIN}/api/v3/exchangeInfo"), 20),
        MarketKind::LinearPerps => (format!("{LINEAR_PERP_DOMAIN}/fapi/v1/exchangeInfo"), 1),
        MarketKind::InversePerps => (format!("{INVERSE_PERP_DOMAIN}/dapi/v1/exchangeInfo"), 1),
    };

    let response_text = hub.http_text_with_limiter(&url, weight, None, None).await?;

    let exchange_info: Value = serde_json::from_str(&response_text)
        .map_err(|e| AdapterError::ParseError(format!("Failed to parse exchange info: {e}")))?;

    let symbols = exchange_info["symbols"]
        .as_array()
        .ok_or_else(|| AdapterError::ParseError("Missing symbols array".to_string()))?;

    let exchange = exchange_from_market_type(market);
    let mut ticker_info_map = HashMap::new();

    for item in symbols {
        let symbol_str = item["symbol"]
            .as_str()
            .ok_or_else(|| AdapterError::ParseError("Missing symbol".to_string()))?;

        if !exchange.is_symbol_supported(symbol_str, true) {
            continue;
        }

        if let Some(contract_type) = item["contractType"].as_str()
            && contract_type != "PERPETUAL"
        {
            continue;
        }
        if let Some(quote_asset) = item["quoteAsset"].as_str()
            && quote_asset != "USDT"
            && quote_asset != "USDC"
            && quote_asset != "USD"
        {
            continue;
        }
        if let Some(status) = item["status"].as_str()
            && status != "TRADING"
            && status != "HALT"
        {
            continue;
        }

        let filters = item["filters"]
            .as_array()
            .ok_or_else(|| AdapterError::ParseError("Missing filters array".to_string()))?;

        let price_filter = filters
            .iter()
            .find(|x| x["filterType"].as_str().unwrap_or_default() == "PRICE_FILTER");

        let min_qty = filters
            .iter()
            .find(|x| x["filterType"].as_str().unwrap_or_default() == "LOT_SIZE")
            .ok_or_else(|| {
                AdapterError::ParseError("Missing minQty in LOT_SIZE filter".to_string())
            })
            .and_then(|x| {
                serde_util::value_as_f32(&x["minQty"])
                    .ok_or_else(|| AdapterError::ParseError("Failed to parse minQty".to_string()))
            })?;

        let contract_size = serde_util::value_as_f32(&item["contractSize"]);

        let ticker = Ticker::new(symbol_str, exchange);

        if let Some(price_filter) = price_filter {
            let min_ticksize = serde_util::value_as_f32(&price_filter["tickSize"])
                .ok_or_else(|| AdapterError::ParseError("tickSize not found".to_string()))?;

            let info = TickerInfo::new(ticker, min_ticksize, min_qty, contract_size);
            ticker_info_map.insert(ticker, Some(info));
        } else {
            ticker_info_map.insert(ticker, None);
        }
    }

    Ok(ticker_info_map)
}

pub(super) async fn fetch_ticker_stats(
    hub: &mut HttpHub<BinanceLimiter>,
    market: MarketKind,
    contract_sizes: Option<&HashMap<Ticker, crate::unit::ContractSize>>,
) -> Result<super::super::TickerStatsMap, AdapterError> {
    let (url, weight) = match market {
        MarketKind::Spot => (format!("{SPOT_DOMAIN}/api/v3/ticker/24hr"), 80),
        MarketKind::LinearPerps => (format!("{LINEAR_PERP_DOMAIN}/fapi/v1/ticker/24hr"), 40),
        MarketKind::InversePerps => (format!("{INVERSE_PERP_DOMAIN}/dapi/v1/ticker/24hr"), 40),
    };

    let parsed_response: Vec<Value> = hub.http_json_with_limiter(&url, weight, None, None).await?;

    let exchange = exchange_from_market_type(market);
    let mut ticker_price_map = HashMap::new();

    for item in parsed_response {
        let symbol = item["symbol"]
            .as_str()
            .ok_or_else(|| AdapterError::ParseError("Symbol not found".to_string()))?;

        if !exchange.is_symbol_supported(symbol, false) {
            continue;
        }

        let ticker = Ticker::new(symbol, exchange);

        let last_price = serde_util::value_as_f64(&item["lastPrice"])
            .ok_or_else(|| AdapterError::ParseError("Last price not found".to_string()))?;

        let price_change_pt =
            serde_util::value_as_f32(&item["priceChangePercent"]).ok_or_else(|| {
                AdapterError::ParseError("Price change percent not found".to_string())
            })?;

        let volume = match market {
            MarketKind::Spot | MarketKind::LinearPerps => {
                serde_util::value_as_f64(&item["quoteVolume"])
                    .or_else(|| serde_util::value_as_f64(&item["volume"]))
            }
            _ => serde_util::value_as_f64(&item["volume"]),
        }
        .ok_or_else(|| AdapterError::ParseError("Volume not found".to_string()))?;

        let daily_volume = match market {
            MarketKind::Spot | MarketKind::LinearPerps => Qty::from_f64(volume),
            MarketKind::InversePerps => {
                let contract_size = match contract_sizes
                    .and_then(|sizes| sizes.get(&ticker))
                    .copied()
                {
                    Some(size) => size.as_f64(),
                    None => {
                        log::debug!("Missing contract size for {ticker}, skipping ticker in stats");
                        continue;
                    }
                };

                Qty::from_f64(volume * contract_size)
            }
        };

        let ticker_stats = TickerStats {
            mark_price: Price::from_f64(last_price),
            daily_price_chg: price_change_pt,
            daily_volume,
        };

        ticker_price_map.insert(ticker, ticker_stats);
    }

    Ok(ticker_price_map)
}

pub(super) async fn fetch_klines(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(UnixMs, UnixMs)>,
) -> Result<Vec<Kline>, AdapterError> {
    let ticker = ticker_info.ticker;

    let (symbol_str, market_type) = ticker.to_full_symbol_and_type();
    let timeframe_str = timeframe.to_string();

    let base_url = match market_type {
        MarketKind::Spot => format!("{SPOT_DOMAIN}/api/v3/klines"),
        MarketKind::LinearPerps => format!("{LINEAR_PERP_DOMAIN}/fapi/v1/klines"),
        MarketKind::InversePerps => format!("{INVERSE_PERP_DOMAIN}/dapi/v1/klines"),
    };

    let mut url = format!("{base_url}?symbol={symbol_str}&interval={timeframe_str}");

    let limit_param = if let Some((start, end)) = range {
        let start = start.as_u64();
        let end = end.as_u64();
        let interval_ms = timeframe.to_milliseconds();
        let num_intervals = ((end - start) / interval_ms).min(1000);

        if num_intervals < 3 {
            let new_start = start - (interval_ms * 5);
            let new_end = end + (interval_ms * 5);
            let num_intervals = ((new_end - new_start) / interval_ms).min(1000);

            url.push_str(&format!(
                "&startTime={new_start}&endTime={new_end}&limit={num_intervals}"
            ));
            num_intervals
        } else {
            url.push_str(&format!(
                "&startTime={start}&endTime={end}&limit={num_intervals}"
            ));
            num_intervals
        }
    } else {
        let num_intervals = 400;
        url.push_str(&format!("&limit={num_intervals}"));
        num_intervals
    };

    let weight = match market_type {
        MarketKind::Spot => 2,
        MarketKind::LinearPerps | MarketKind::InversePerps => match limit_param {
            1..=100 => 1,
            101..=500 => 2,
            501..=1000 => 5,
            1001..=1500 => 10,
            _ => {
                return Err(AdapterError::InvalidRequest(format!(
                    "Unsupported kline limit parameter for perps market: {limit_param}"
                )));
            }
        },
    };

    let fetched_klines: Vec<FetchedKline> =
        hub.http_json_with_limiter(&url, weight, None, None).await?;

    let size_in_quote_ccy = volume_size_unit() == SizeUnit::Quote;
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        size_in_quote_ccy,
        ticker_info,
        raw_qty_unit_from_market_type(market_type),
    );
    let min_ticksize = ticker_info.min_ticksize;

    let klines: Vec<_> = fetched_klines
        .into_iter()
        .map(|k| {
            let FetchedKline(
                time,
                open,
                high,
                low,
                close,
                volume,
                _close_time,
                _quote_asset_volume,
                _number_of_trades,
                taker_buy_base_asset_volume,
                _taker_buy_quote_asset_volume,
                _ignore,
            ) = k;

            let buy_volume = taker_buy_base_asset_volume;
            let sell_volume = volume - buy_volume;

            let buy_volume = qty_norm.normalize_qty(buy_volume, close);
            let sell_volume = qty_norm.normalize_qty(sell_volume, close);

            Kline::new(
                time,
                open,
                high,
                low,
                close,
                Volume::BuySell(buy_volume, sell_volume),
                min_ticksize,
            )
        })
        .collect();

    Ok(klines)
}

fn oi_range_reaches_live_edge(
    range: Option<(UnixMs, UnixMs)>,
    requested_period: Timeframe,
    now: UnixMs,
) -> bool {
    range.is_none_or(|(_, end)| {
        end.saturating_add(requested_period.to_milliseconds().max(60_000) * 2) >= now
    })
}

async fn fetch_current_oi(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
) -> Result<OpenInterest, AdapterError> {
    let (ticker_str, market) = ticker_info.ticker.to_full_symbol_and_type();
    let (url, weight) = match market {
        MarketKind::LinearPerps => (
            format!("{LINEAR_PERP_DOMAIN}/fapi/v1/openInterest?symbol={ticker_str}"),
            1,
        ),
        MarketKind::InversePerps => (
            format!("{INVERSE_PERP_DOMAIN}/dapi/v1/openInterest?symbol={ticker_str}"),
            1,
        ),
        MarketKind::Spot => {
            return Err(AdapterError::InvalidRequest(
                "Open interest is unavailable for Binance spot markets".to_string(),
            ));
        }
    };
    let snapshot: DeCurrentOpenInterest =
        hub.http_json_with_limiter(&url, weight, None, None).await?;
    let value = match market {
        MarketKind::LinearPerps => {
            let mark_url = format!("{LINEAR_PERP_DOMAIN}/fapi/v1/premiumIndex?symbol={ticker_str}");
            let mark: DePremiumIndex = hub.http_json_with_limiter(&mark_url, 1, None, None).await?;
            snapshot.open_interest * mark.mark_price
        }
        MarketKind::InversePerps => {
            let contract_size = ticker_info.contract_size.ok_or_else(|| {
                AdapterError::ParseError(format!(
                    "Missing contract size for Binance inverse OI {ticker_str}"
                ))
            })?;
            snapshot.open_interest * contract_size.as_f64()
        }
        MarketKind::Spot => unreachable!("spot open interest was rejected above"),
    };
    if !value.is_finite() || value < 0.0 {
        return Err(AdapterError::ParseError(format!(
            "Invalid current Binance OI for {ticker_str}: {value}"
        )));
    }
    Ok(OpenInterest {
        time: UnixMs::new(snapshot.time),
        value,
    })
}

pub(super) async fn fetch_historical_oi(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    range: Option<(UnixMs, UnixMs)>,
    period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    let (ticker_str, market) = ticker_info.ticker.to_full_symbol_and_type();
    // Binance's downloadable OI history starts at 5m. Sub-5m charts combine
    // that native history with exact current snapshots archived by the
    // connector; no 1m points are synthesized from these 5m rows.
    let historical_period = match period {
        Timeframe::M1 | Timeframe::M3 => Timeframe::M5,
        other => other,
    };
    let period_str = historical_period.to_string();

    let (base_url, pair_str, weight) = match market {
        MarketKind::LinearPerps => (
            format!("{LINEAR_PERP_DOMAIN}/futures/data/openInterestHist"),
            format!("?symbol={ticker_str}"),
            12,
        ),
        MarketKind::InversePerps => {
            let Some((pair, _)) = ticker_str.split_once('_') else {
                let err_msg =
                    format!("Unsupported inverse ticker format for open interest: {ticker_str}");
                log::error!("{err_msg}");
                return Err(AdapterError::InvalidRequest(err_msg));
            };

            (
                format!("{INVERSE_PERP_DOMAIN}/futures/data/openInterestHist"),
                format!("?pair={pair}&contractType=PERPETUAL"),
                1,
            )
        }
        _ => {
            let err_msg = format!("Unsupported market type for open interest: {market:?}");
            log::error!("{}", err_msg);
            return Err(AdapterError::InvalidRequest(err_msg));
        }
    };

    let mut url = format!("{base_url}{pair_str}&period={period_str}");

    if let Some((start, end)) = range {
        let start = start.as_u64();
        let end = end.as_u64();
        let now_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| {
                AdapterError::ParseError(format!("System clock before UNIX_EPOCH: {err}"))
            })?
            .as_millis() as u64;
        let thirty_days_ago = now_ms.saturating_sub(THIRTY_DAYS_MS);

        if end < thirty_days_ago {
            let err_msg = format!(
                "Requested end time {end} is before available data (30 days is the API limit)"
            );
            log::error!("{}", err_msg);
            return Err(AdapterError::InvalidRequest(err_msg));
        }

        let adjusted_start = if start < thirty_days_ago {
            log::warn!(
                "Adjusting start time from {} to {} (30 days limit)",
                start,
                thirty_days_ago
            );
            thirty_days_ago
        } else {
            start
        };

        let interval_ms = historical_period.to_milliseconds();
        // Ranges shorter than one bucket (e.g. the developing-candle OI
        // re-poll) must still request at least one sample: Binance rejects
        // `limit=0` with HTTP 400.
        let num_intervals = (end.saturating_sub(adjusted_start) / interval_ms).clamp(1, 500);

        url.push_str(&format!(
            "&startTime={adjusted_start}&endTime={end}&limit={num_intervals}"
        ));
    } else {
        url.push_str("&limit=400");
    }

    let binance_oi: Vec<DeOpenInterest> =
        hub.http_json_with_limiter(&url, weight, None, None).await?;

    let contract_size = ticker_info.contract_size;
    let mut open_interest = binance_oi
        .iter()
        .map(|x| {
            let value = match market {
                MarketKind::LinearPerps => x
                    .notional
                    .as_deref()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| {
                        AdapterError::ParseError(format!(
                            "Missing USD notional in Binance OI history for {ticker_str} at {}",
                            x.time
                        ))
                    })?,
                MarketKind::InversePerps => {
                    let contract_size = contract_size.ok_or_else(|| {
                        AdapterError::ParseError(format!(
                            "Missing contract size for Binance inverse OI {ticker_str}"
                        ))
                    })?;
                    x.sum * contract_size.as_f64()
                }
                MarketKind::Spot => unreachable!("spot open interest was rejected above"),
            };
            if !value.is_finite() || value < 0.0 {
                return Err(AdapterError::ParseError(format!(
                    "Invalid Binance OI for {ticker_str} at {}: {value}",
                    x.time
                )));
            }
            // Binance timestamps history at the completed interval's right
            // edge. Keep that close in the interval it summarizes; live
            // snapshots below retain their exact observation time.
            Ok(OpenInterest::completed_interval(x.time.into(), value))
        })
        .collect::<Result<Vec<_>, AdapterError>>()?;

    let now = UnixMs::now();
    if oi_range_reaches_live_edge(range, period, now) {
        match fetch_current_oi(hub, ticker_info).await {
            Ok(snapshot) => open_interest.push(snapshot),
            Err(err) if open_interest.is_empty() => return Err(err),
            Err(err) => log::warn!(
                "Failed to refresh current Binance OI for {ticker_str}; using native history: {err}"
            ),
        }
    }

    let mut deduplicated = BTreeMap::new();
    for point in open_interest {
        deduplicated.insert(point.time, point.value);
    }
    Ok(deduplicated
        .into_iter()
        .map(|(time, value)| OpenInterest { time, value })
        .collect())
}

fn aggtrades_request_weight(market: MarketKind) -> usize {
    match market {
        MarketKind::Spot => 4,
        MarketKind::LinearPerps | MarketKind::InversePerps => 20,
    }
}

fn aggtrades_base_url(market: MarketKind) -> String {
    match market {
        MarketKind::Spot => format!("{SPOT_DOMAIN}/api/v3/aggTrades"),
        MarketKind::LinearPerps => format!("{LINEAR_PERP_DOMAIN}/fapi/v1/aggTrades"),
        MarketKind::InversePerps => format!("{INVERSE_PERP_DOMAIN}/dapi/v1/aggTrades"),
    }
}

fn map_de_trades(
    de_trades: Vec<DeTrade>,
    ticker_info: TickerInfo,
    market_type: MarketKind,
) -> Vec<Trade> {
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        volume_size_unit() == SizeUnit::Quote,
        ticker_info,
        raw_qty_unit_from_market_type(market_type),
    );

    de_trades
        .into_iter()
        .map(|de_trade| Trade {
            time: de_trade.time.into(),
            is_sell: de_trade.is_sell,
            price: Price::from_f64(de_trade.price).round_to_min_tick(ticker_info.min_ticksize),
            qty: qty_norm.normalize_qty(de_trade.qty, de_trade.price),
        })
        .collect()
}

async fn fetch_intraday_trades(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from: UnixMs,
    to: Option<UnixMs>,
) -> Result<Vec<Trade>, AdapterError> {
    let (symbol_str, market_type) = ticker_info.ticker.to_full_symbol_and_type();

    let mut url = format!(
        "{}?symbol={symbol_str}&limit=1000",
        aggtrades_base_url(market_type)
    );
    if let Some(to) = to {
        // Supplying only endTime asks Binance for the newest page ending at
        // this cursor. This lets callers page backward from the live edge.
        url.push_str(&format!("&endTime={}", to.as_u64()));
    } else {
        url.push_str(&format!("&startTime={}", from.as_u64()));
    }

    let de_trades: Vec<DeTrade> = hub
        .http_json_with_limiter(&url, aggtrades_request_weight(market_type), None, None)
        .await?;

    Ok(map_de_trades(de_trades, ticker_info, market_type))
}

/// Binance caps an aggTrades page at 1000 rows; a full page means the slice
/// may hold more trades and must be split again.
const AGGTRADES_PAGE_LIMIT: usize = 1000;
/// How many slice requests to keep in flight per wave. At weight 20 per
/// aggTrades page this is 200 weight per wave — well inside the ~2300/min
/// effective perps budget.
const PARALLEL_SLICE_WAVE: usize = 10;
/// Slices narrower than this are never split further — a capped page this
/// small is kept as-is instead of recursing forever. Continuation then uses
/// `fromId` so the burst is still fully read.
const MIN_SLICE_SPAN_MS: u64 = 1_000;
/// Binance requires `endTime - startTime` to be strictly less than 1 hour.
const MAX_AGGTRADES_WINDOW_MS: u64 = 60 * 60 * 1_000 - 1;

/// Split the inclusive range `[from, to]` into `[from, mid]` and
/// `[mid + 1, to]`. Returns `(mid, mid + 1)` so both sub-ranges stay disjoint.
fn split_intraday_slice(from: UnixMs, to: UnixMs) -> Option<(UnixMs, UnixMs)> {
    let from_ms = from.as_u64();
    let to_ms = to.as_u64();
    if to_ms <= from_ms || to_ms - from_ms < MIN_SLICE_SPAN_MS {
        return None;
    }
    let mid = from_ms + (to_ms - from_ms) / 2;
    Some((UnixMs::new(mid), UnixMs::new(mid + 1)))
}

/// Inclusive `[from, to]` windows no longer than Binance's aggTrades limit
/// (`startTime`/`endTime` must be strictly less than one hour apart). A
/// single `[day_start, now]` request is rejected or silently truncated,
/// which punched holes in Large Trades during the busy part of the session.
fn hour_windows(from: UnixMs, to: UnixMs) -> Vec<(UnixMs, UnixMs)> {
    if to < from {
        return Vec::new();
    }
    let mut windows = Vec::new();
    let mut cursor = from.as_u64();
    let end = to.as_u64();
    while cursor <= end {
        let window_end = cursor.saturating_add(MAX_AGGTRADES_WINDOW_MS).min(end);
        windows.push((UnixMs::new(cursor), UnixMs::new(window_end)));
        if window_end >= end {
            break;
        }
        cursor = window_end.saturating_add(1);
    }
    windows
}

async fn fetch_intraday_page(
    hub: &HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from: UnixMs,
    to: UnixMs,
) -> Result<Vec<DeTrade>, AdapterError> {
    let (symbol_str, market_type) = ticker_info.ticker.to_full_symbol_and_type();

    let url = format!(
        "{}?symbol={symbol_str}&limit=1000&startTime={}&endTime={}",
        aggtrades_base_url(market_type),
        from.as_u64(),
        to.as_u64()
    );

    hub.http_json_with_limiter(&url, aggtrades_request_weight(market_type), None, None)
        .await
}

async fn fetch_intraday_page_from_id(
    hub: &HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from_id: u64,
) -> Result<Vec<DeTrade>, AdapterError> {
    let (symbol_str, market_type) = ticker_info.ticker.to_full_symbol_and_type();

    // Binance times out if fromId is combined with startTime/endTime.
    let url = format!(
        "{}?symbol={symbol_str}&limit=1000&fromId={from_id}",
        aggtrades_base_url(market_type)
    );

    hub.http_json_with_limiter(&url, aggtrades_request_weight(market_type), None, None)
        .await
}

/// Keep paging an unsplittable (sub-second) window with `fromId` so a
/// liquidation burst that prints more than 1000 aggTrades is not truncated
/// to the first page — that was dropping the large prints in the timespot.
async fn fetch_intraday_from_id_until(
    hub: &HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    mut from_id: u64,
    until: UnixMs,
) -> Result<Vec<Trade>, AdapterError> {
    let market_type = ticker_info.market_type();
    let until_ms = until.as_u64();
    let mut merged = Vec::new();

    loop {
        let page = fetch_intraday_page_from_id(hub, ticker_info, from_id).await?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let mut last_id = from_id;
        let mut past_end = false;
        let mut kept = Vec::with_capacity(page_len);
        for de_trade in page {
            last_id = de_trade.id;
            if de_trade.time > until_ms {
                past_end = true;
                break;
            }
            kept.push(de_trade);
        }
        merged.extend(map_de_trades(kept, ticker_info, market_type));
        if past_end || page_len < AGGTRADES_PAGE_LIMIT || last_id < from_id {
            break;
        }
        let next_id = last_id.saturating_add(1);
        if next_id <= from_id {
            break;
        }
        from_id = next_id;
    }

    Ok(merged)
}

/// Fetch today's trades by fetching several time slices concurrently instead
/// of walking a serial cursor one 1000-row page at a time. Slices whose page
/// comes back full are split in half and requeued until either the data fits
/// or the minimum span is reached. A full page that cannot be split is then
/// continued with `fromId` so busy milliseconds are not silently truncated.
async fn fetch_intraday_trades_parallel(
    hub: &HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from: UnixMs,
    to: UnixMs,
) -> Result<Vec<Trade>, AdapterError> {
    let market_type = ticker_info.market_type();
    let mut merged: Vec<Trade> = Vec::new();
    let mut pending: Vec<(UnixMs, UnixMs)> = hour_windows(from, to);

    while !pending.is_empty() {
        let wave_len = pending.len().min(PARALLEL_SLICE_WAVE);
        let wave: Vec<(UnixMs, UnixMs)> = pending.drain(..wave_len).collect();

        let pages = futures::future::join_all(wave.iter().map(|(slice_from, slice_to)| {
            fetch_intraday_page(hub, ticker_info, *slice_from, *slice_to)
        }))
        .await;

        for ((slice_from, slice_to), page) in wave.into_iter().zip(pages) {
            let de_trades = page?;
            if de_trades.len() >= AGGTRADES_PAGE_LIMIT {
                if let Some((mid, next)) = split_intraday_slice(slice_from, slice_to) {
                    // Full page — likely truncated. Requeue the halves rather
                    // than keeping a capped window.
                    pending.insert(0, (next, slice_to));
                    pending.insert(0, (slice_from, mid));
                    continue;
                }
                let last_id = de_trades.last().map(|trade| trade.id).unwrap_or(0);
                merged.extend(map_de_trades(de_trades, ticker_info, market_type));
                if last_id > 0 {
                    merged.extend(
                        fetch_intraday_from_id_until(
                            hub,
                            ticker_info,
                            last_id.saturating_add(1),
                            slice_to,
                        )
                        .await?,
                    );
                }
                continue;
            }
            merged.extend(map_de_trades(de_trades, ticker_info, market_type));
        }
    }

    merged.sort_by_key(|trade| trade.time);
    // Slice boundaries are disjoint and fromId continuation starts after the
    // last aggregate-trade ID. Do not value-deduplicate here: two legitimate
    // executions can share the same millisecond, price, quantity, and side.
    Ok(merged)
}

/// Intraday fetch used while paging forward into today. With a known upper
/// bound the parallel slicer is used; without one the single-cursor page is
/// returned so callers can keep advancing.
async fn fetch_intraday_range(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from: UnixMs,
    to: Option<UnixMs>,
) -> Result<Vec<Trade>, AdapterError> {
    match to {
        Some(to) if to >= from => fetch_intraday_trades_parallel(hub, ticker_info, from, to).await,
        _ => fetch_intraday_trades(hub, ticker_info, from, None).await,
    }
}

/// Hard cap per archive read. Busy BTCUSDT days are ~0.5â€“2M aggTrades; 500k
/// truncated the session high/low used by Daily Delta. Still bounded so a
/// pathological file cannot grow without limit.
const MAX_ARCHIVE_TRADES_PER_FETCH: usize = 2_000_000;

async fn get_hist_trades_with_client(
    client: &reqwest::Client,
    ticker_info: TickerInfo,
    date: chrono::NaiveDate,
    base_path: PathBuf,
    from_time: UnixMs,
    until: Option<UnixMs>,
    limit: usize,
) -> Result<Vec<Trade>, AdapterError> {
    let ticker = ticker_info.ticker;
    let (symbol, market_type) = ticker.to_full_symbol_and_type();

    let market_subpath = match market_type {
        MarketKind::Spot => format!("data/spot/daily/aggTrades/{symbol}"),
        MarketKind::LinearPerps => format!("data/futures/um/daily/aggTrades/{symbol}"),
        MarketKind::InversePerps => format!("data/futures/cm/daily/aggTrades/{symbol}"),
    };

    let zip_file_name = format!(
        "{}-aggTrades-{}.zip",
        symbol.to_uppercase(),
        date.format("%Y-%m-%d"),
    );

    let base_path = base_path.join(&market_subpath);

    std::fs::create_dir_all(&base_path)
        .map_err(|e| AdapterError::ParseError(format!("Failed to create directories: {e}")))?;

    let zip_path = format!("{market_subpath}/{zip_file_name}");
    let base_zip_path = base_path.join(&zip_file_name);
    // Negative cache: Binance often lags 1 day publishing daily zips. Without
    // this marker, paging re-requests the same 404 on every REST page and can
    // hang/crash the app in a download storm.
    let missing_marker_path = base_path.join(format!("{zip_file_name}.missing"));

    if missing_marker_path.exists() {
        // Re-check periodically — Binance often publishes yesterday's zip a
        // few hours after midnight UTC. A short window keeps the original
        // download-storm protection while ensuring an early 404 (archive not
        // published yet) does not hide the day for the rest of the session.
        let marker_is_fresh = std::fs::metadata(&missing_marker_path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < std::time::Duration::from_secs(60 * 60));
        if marker_is_fresh {
            return Err(AdapterError::InvalidRequest(format!(
                "Archive previously unavailable (404): {zip_path}"
            )));
        }
        let _ = std::fs::remove_file(&missing_marker_path);
    }

    if std::fs::metadata(&base_zip_path).is_ok() {
        log::info!("Using cached {}", zip_path);
    } else {
        let url = format!("https://data.binance.vision/{zip_path}");

        log::info!("Downloading from {}", url);

        let resp = client.get(&url).send().await.map_err(AdapterError::from)?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Remember the miss for this session's data dir so paging does not
            // hammer data.binance.vision for the same missing day.
            if let Err(e) = std::fs::write(&missing_marker_path, b"") {
                log::warn!("Failed to write missing-archive marker {missing_marker_path:?}: {e}");
            }
            return Err(AdapterError::InvalidRequest(format!(
                "Archive not found (404): {url}"
            )));
        }

        if !resp.status().is_success() {
            return Err(AdapterError::InvalidRequest(format!(
                "Failed to fetch from {}: {}",
                url,
                resp.status()
            )));
        }

        let body = resp.bytes().await.map_err(AdapterError::from)?;

        std::fs::write(&base_zip_path, &body).map_err(|e| {
            AdapterError::ParseError(format!("Failed to write zip file: {e}, {base_zip_path:?}"))
        })?;
        // Successful download supersedes any stale miss marker.
        let _ = std::fs::remove_file(&missing_marker_path);
    }

    let file = std::fs::File::open(&base_zip_path)
        .map_err(|e| AdapterError::ParseError(format!("Failed to open compressed file: {e}")))?;

    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AdapterError::ParseError(format!("Failed to unzip file: {e}")))?;

    let qty_norm = QtyNormalization::with_raw_qty_unit(
        volume_size_unit() == SizeUnit::Quote,
        ticker_info,
        raw_qty_unit_from_market_type(market_type),
    );

    // Stream the CSV and keep only a bounded page starting at `from_time`.
    // Never materialize a full BTCUSDT day in memory.
    let mut trades = Vec::with_capacity(limit.min(MAX_ARCHIVE_TRADES_PER_FETCH));
    let from_ms = from_time.as_u64();
    let until_ms = until.map(|time| time.as_u64());

    for i in 0..archive.len() {
        if trades.len() >= limit {
            break;
        }

        let csv_file = archive
            .by_index(i)
            .map_err(|e| AdapterError::ParseError(format!("Failed to read csv: {e}")))?;

        let mut csv_reader = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(BufReader::new(csv_file));

        for record in csv_reader.records() {
            let Ok(record) = record else {
                continue;
            };
            let Ok(time) = record[5].parse::<u64>() else {
                continue;
            };
            if time < from_ms {
                continue;
            }
            if until_ms.is_some_and(|until| time > until) {
                break;
            }

            let Ok(is_sell) = record[6].parse::<bool>() else {
                continue;
            };
            let Ok(price_f64) = record[1].parse::<f64>() else {
                continue;
            };
            let Ok(qty_f64) = record[2].parse::<f64>() else {
                continue;
            };

            let price = Price::from_f64(price_f64).round_to_min_tick(ticker_info.min_ticksize);
            let qty = qty_norm.normalize_qty(qty_f64, price_f64);

            trades.push(Trade {
                time: time.into(),
                is_sell,
                price,
                qty,
            });

            if trades.len() >= limit {
                break;
            }
        }
    }

    Ok(trades)
}

pub(super) async fn fetch_trades(
    hub: &mut HttpHub<BinanceLimiter>,
    ticker_info: TickerInfo,
    from_time: UnixMs,
    to_time: Option<UnixMs>,
    data_path: Option<PathBuf>,
) -> Result<Vec<Trade>, AdapterError> {
    let Some(data_path) = data_path else {
        return Err(AdapterError::InvalidRequest(
            "Binance trades fetch requires data_path".to_string(),
        ));
    };

    let today_date = chrono::Utc::now().date_naive();
    let today_midnight = today_date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| {
            AdapterError::ParseError("Failed to construct UTC midnight timestamp".to_string())
        })?
        .and_utc();

    let from_time_ms = i64::try_from(from_time.as_u64())
        .map_err(|_| AdapterError::InvalidRequest("Timestamp exceeds i64 range".to_string()))?;

    if from_time_ms >= today_midnight.timestamp_millis() {
        // Forward paging uses startTime; `to_time` is enforced by the caller.
        return fetch_intraday_range(hub, ticker_info, from_time, to_time).await;
    }

    let mut cursor_time = from_time;
    let mut cursor_date = chrono::DateTime::from_timestamp_millis(from_time_ms)
        .ok_or_else(|| AdapterError::ParseError("Invalid timestamp".into()))?
        .date_naive();

    let client = hub.client().clone();

    // Walk completed UTC days until we produce a non-empty page or reach today.
    // Each page is capped so BTCUSDT archives cannot OOM the process.
    //
    // Binance REST aggTrades only covers ~2 recent days (`-4166` otherwise), so
    // a missing/exhausted archive must not fall through to REST for older days.
    // Return an empty page and let the caller stop; yesterday/today still REST.
    //
    // Archive days are downloaded in small concurrent windows; results are
    // processed strictly in order, so paging semantics match a serial walk.
    const ARCHIVE_PREFETCH_DAYS: usize = 4;

    while cursor_date < today_date {
        if to_time.is_some_and(|until| cursor_time > until) {
            return Ok(Vec::new());
        }

        let mut window: Vec<(chrono::NaiveDate, UnixMs)> = Vec::new();
        {
            let mut probe_date = cursor_date;
            let mut probe_time = cursor_time;
            while probe_date < today_date
                && window.len() < ARCHIVE_PREFETCH_DAYS
                && to_time.is_none_or(|until| probe_time <= until)
            {
                window.push((probe_date, probe_time));
                let Some(next_date) = probe_date.succ_opt() else {
                    break;
                };
                let Some(next_midnight) = next_date.and_hms_opt(0, 0, 0) else {
                    break;
                };
                probe_time = UnixMs::new(
                    u64::try_from(next_midnight.and_utc().timestamp_millis()).unwrap_or(u64::MAX),
                );
                probe_date = next_date;
            }
        }

        let downloads = window.iter().map(|(window_date, window_from)| {
            let client = client.clone();
            let data_path = data_path.clone();
            async move {
                get_hist_trades_with_client(
                    &client,
                    ticker_info,
                    *window_date,
                    data_path,
                    *window_from,
                    to_time,
                    MAX_ARCHIVE_TRADES_PER_FETCH,
                )
                .await
            }
        });
        let results = futures::future::join_all(downloads).await;

        for ((window_date, window_from), result) in window.into_iter().zip(results) {
            match result {
                Ok(batch) if !batch.is_empty() => {
                    return Ok(batch);
                }
                Ok(_) => {
                    // Archive day fully behind the cursor - advance to next midnight.
                }
                Err(e) => {
                    log::warn!("Historical trades archive unavailable for {window_date}: {e}");
                    if today_date.signed_duration_since(window_date).num_days() <= 2 {
                        return fetch_intraday_range(hub, ticker_info, window_from, to_time).await;
                    }
                    return Ok(Vec::new());
                }
            }

            cursor_date = window_date
                .checked_add_signed(chrono::Duration::days(1))
                .ok_or_else(|| {
                    AdapterError::ParseError("Date overflow while paging trades".into())
                })?;
            let next_midnight_ms = cursor_date
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| AdapterError::ParseError("Failed to construct day boundary".into()))?
                .and_utc()
                .timestamp_millis();
            cursor_time = UnixMs::new(u64::try_from(next_midnight_ms).unwrap_or(u64::MAX));
        }
    }

    fetch_intraday_range(hub, ticker_info, cursor_time, to_time).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_split_keeps_subranges_disjoint_and_covering() {
        let from = UnixMs::new(1_000);
        let to = UnixMs::new(9_000);
        let (mid, next) = split_intraday_slice(from, to).expect("splittable range");

        assert_eq!(mid.as_u64(), 5_000);
        assert_eq!(next.as_u64(), 5_001);

        // Left: [from, mid], right: [next, to] - no gap, no overlap.
        assert!(mid >= from && next <= to);
        assert_eq!(next.as_u64() - mid.as_u64(), 1);
    }

    #[test]
    fn slices_below_minimum_span_are_never_split() {
        let from = UnixMs::new(1_000);
        let to = UnixMs::new(1_000 + MIN_SLICE_SPAN_MS - 1);

        assert!(split_intraday_slice(from, to).is_none());
        assert!(split_intraday_slice(to, from).is_none()); // inverted range
    }

    #[test]
    fn minimum_span_slices_still_split_at_the_boundary() {
        let from = UnixMs::new(0);
        let to = UnixMs::new(MIN_SLICE_SPAN_MS);

        let (mid, next) = split_intraday_slice(from, to).expect("exactly minimum span");
        assert_eq!(mid.as_u64(), MIN_SLICE_SPAN_MS / 2);
        assert_eq!(next.as_u64(), MIN_SLICE_SPAN_MS / 2 + 1);
    }

    #[test]
    fn hour_windows_never_exceed_binance_aggtrades_limit() {
        let from = UnixMs::new(0);
        let to = UnixMs::new(3 * 60 * 60 * 1_000 + 12_345);
        let windows = hour_windows(from, to);

        assert!(
            windows.len() >= 4,
            "a 3h+ span must be more than one window"
        );
        assert_eq!(windows.first().map(|(start, _)| *start), Some(from));
        assert_eq!(windows.last().map(|(_, end)| *end), Some(to));
        for (start, end) in &windows {
            assert!(end >= start);
            assert!(
                end.as_u64().saturating_sub(start.as_u64()) <= MAX_AGGTRADES_WINDOW_MS,
                "window {start:?}..{end:?} is too wide for aggTrades"
            );
        }
        for pair in windows.windows(2) {
            assert_eq!(pair[1].0.as_u64(), pair[0].1.as_u64().saturating_add(1));
        }
    }

    #[test]
    fn short_intraday_range_stays_a_single_window() {
        let from = UnixMs::new(1_000);
        let to = UnixMs::new(1_000 + 10 * 60 * 1_000);
        let windows = hour_windows(from, to);
        assert_eq!(windows, vec![(from, to)]);
    }
}
