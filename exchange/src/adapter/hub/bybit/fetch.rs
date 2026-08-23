use crate::{
    Kline, OpenInterest, Price, Qty, Ticker, TickerInfo, TickerStats, Timeframe, Trade, UnixMs,
    adapter::hub::TickerMetadataMap,
    serde_util,
    unit::qty::{QtyNormalization, SizeUnit, volume_size_unit},
};
use csv::ReaderBuilder;
use flate2::read::GzDecoder;
use std::{io::BufReader, path::PathBuf};

use super::{
    BybitLimiter, FETCH_DOMAIN, HttpHub, MarketKind, exchange_from_market_type,
    raw_qty_unit_from_market_type,
};
use crate::adapter::hub::AdapterError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeOpenInterest {
    #[serde(
        rename = "openInterest",
        deserialize_with = "serde_util::de_string_to_number"
    )]
    pub value: f64,
    #[serde(rename = "singleOpenInterest", default)]
    pub single_side_value: Option<String>,
    #[serde(deserialize_with = "serde_util::de_string_to_number")]
    pub timestamp: u64,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct ApiResponse {
    #[serde(rename = "retCode")]
    ret_code: u32,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    result: ApiResult,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct ApiResult {
    symbol: String,
    category: String,
    list: Vec<Vec<Value>>,
}

fn parse_kline_field<T: std::str::FromStr>(field: Option<&str>) -> Result<T, AdapterError> {
    field
        .ok_or_else(|| AdapterError::ParseError("Failed to parse kline".to_string()))
        .and_then(|s| {
            s.parse::<T>()
                .map_err(|_| AdapterError::ParseError("Failed to parse kline".to_string()))
        })
}

pub(super) async fn fetch_ticker_metadata(
    hub: &mut HttpHub<BybitLimiter>,
    market_type: MarketKind,
) -> Result<TickerMetadataMap, AdapterError> {
    let exchange = exchange_from_market_type(market_type);

    let market = match market_type {
        MarketKind::Spot => "spot",
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
    };

    let url = format!("{FETCH_DOMAIN}/v5/market/instruments-info?category={market}&limit=1000",);
    let response_text = hub.http_text_with_limiter(&url, 1, None, None).await?;

    let exchange_info: Value =
        sonic_rs::from_str(&response_text).map_err(|e| AdapterError::ParseError(e.to_string()))?;

    let result_list: &Vec<Value> = exchange_info["result"]["list"]
        .as_array()
        .ok_or_else(|| AdapterError::ParseError("Result list is not an array".to_string()))?;

    let mut ticker_info_map = HashMap::new();

    for item in result_list {
        let symbol = item["symbol"]
            .as_str()
            .ok_or_else(|| AdapterError::ParseError("Symbol not found".to_string()))?;

        if !exchange.is_symbol_supported(symbol, true) {
            continue;
        }

        if let Some(contract_type) = item["contractType"].as_str()
            && contract_type != "LinearPerpetual"
            && contract_type != "InversePerpetual"
        {
            continue;
        }

        if let Some(quote_asset) = item["quoteCoin"].as_str()
            && quote_asset != "USDT"
            && quote_asset != "USDC"
            && quote_asset != "USD"
        {
            continue;
        }

        let lot_size_filter = item["lotSizeFilter"]
            .as_object()
            .ok_or_else(|| AdapterError::ParseError("Lot size filter not found".to_string()))?;

        let min_qty = serde_util::value_as_f32(&lot_size_filter["minOrderQty"])
            .ok_or_else(|| AdapterError::ParseError("Min order qty not found".to_string()))?;

        let price_filter = item["priceFilter"]
            .as_object()
            .ok_or_else(|| AdapterError::ParseError("Price filter not found".to_string()))?;

        let min_ticksize = serde_util::value_as_f32(&price_filter["tickSize"])
            .ok_or_else(|| AdapterError::ParseError("Tick size not found".to_string()))?;

        let ticker = Ticker::new(symbol, exchange);
        let info = TickerInfo::new(ticker, min_ticksize, min_qty, None);

        ticker_info_map.insert(ticker, Some(info));
    }

    Ok(ticker_info_map)
}

pub(super) async fn fetch_ticker_stats(
    hub: &mut HttpHub<BybitLimiter>,
    market_type: MarketKind,
) -> Result<super::super::TickerStatsMap, AdapterError> {
    let exchange = exchange_from_market_type(market_type);

    let market = match market_type {
        MarketKind::Spot => "spot",
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
    };

    let url = format!("{FETCH_DOMAIN}/v5/market/tickers?category={market}");
    let parsed_response: Value = hub.http_json_with_limiter(&url, 1, None, None).await?;

    let result_list: &Vec<Value> = parsed_response["result"]["list"]
        .as_array()
        .ok_or_else(|| AdapterError::ParseError("Result list is not an array".to_string()))?;

    let mut ticker_prices_map = HashMap::new();

    for item in result_list {
        let symbol = item["symbol"]
            .as_str()
            .ok_or_else(|| AdapterError::ParseError("Symbol not found".to_string()))?;

        if !exchange.is_symbol_supported(symbol, false) {
            continue;
        }

        let mark_price = serde_util::value_as_f64(&item["lastPrice"])
            .ok_or_else(|| AdapterError::ParseError("Mark price not found".to_string()))?;

        let daily_price_chg = serde_util::value_as_f32(&item["price24hPcnt"])
            .ok_or_else(|| AdapterError::ParseError("Daily price change not found".to_string()))?;

        let daily_volume = serde_util::value_as_f64(&item["volume24h"])
            .ok_or_else(|| AdapterError::ParseError("Daily volume not found".to_string()))?;

        let volume_in_usd = if market_type == MarketKind::InversePerps {
            daily_volume
        } else {
            daily_volume * mark_price
        };

        let ticker_stats = TickerStats {
            mark_price: Price::from_f64(mark_price),
            daily_price_chg: daily_price_chg * 100.0,
            daily_volume: Qty::from_f64(volume_in_usd),
        };

        ticker_prices_map.insert(Ticker::new(symbol, exchange), ticker_stats);
    }

    Ok(ticker_prices_map)
}

pub(super) async fn fetch_klines(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(UnixMs, UnixMs)>,
) -> Result<Vec<Kline>, AdapterError> {
    let ticker = ticker_info.ticker;

    let (symbol_str, market_type) = &ticker.to_full_symbol_and_type();
    let timeframe_str = {
        if Timeframe::D1 == timeframe {
            "D".to_string()
        } else {
            timeframe.to_minutes().to_string()
        }
    };

    let market = match market_type {
        MarketKind::Spot => "spot",
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
    };

    let mut url = format!(
        "{FETCH_DOMAIN}/v5/market/kline?category={}&symbol={}&interval={}",
        market,
        symbol_str.to_uppercase(),
        timeframe_str
    );

    if let Some((start, end)) = range {
        let start = start.as_u64();
        let end = end.as_u64();
        let interval_ms = timeframe.to_milliseconds();
        let num_intervals = ((end - start) / interval_ms).min(1000);

        url.push_str(&format!("&start={start}&end={end}&limit={num_intervals}"));
    }

    let response: ApiResponse = hub.http_json_with_limiter(&url, 1, None, None).await?;

    let size_in_quote_ccy = volume_size_unit() == SizeUnit::Quote;
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        size_in_quote_ccy,
        ticker_info,
        raw_qty_unit_from_market_type(*market_type),
    );

    let klines: Result<Vec<Kline>, AdapterError> = response
        .result
        .list
        .iter()
        .map(|kline| {
            let time = parse_kline_field::<u64>(kline[0].as_str())?;

            let open = parse_kline_field::<f64>(kline[1].as_str())?;
            let high = parse_kline_field::<f64>(kline[2].as_str())?;
            let low = parse_kline_field::<f64>(kline[3].as_str())?;
            let close = parse_kline_field::<f64>(kline[4].as_str())?;

            let volume = parse_kline_field::<f64>(kline[5].as_str())?;
            let volume = qty_norm.normalize_qty(volume, close);

            let kline = Kline::new(
                time,
                open,
                high,
                low,
                close,
                crate::Volume::TotalOnly(volume),
                ticker_info.min_ticksize,
            );

            Ok(kline)
        })
        .collect();

    klines
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

async fn fetch_mark_price_closes(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
    period: Timeframe,
    range: Option<(UnixMs, UnixMs)>,
) -> Result<BTreeMap<UnixMs, f64>, AdapterError> {
    let (ticker_str, market) = ticker_info.ticker.to_full_symbol_and_type();
    let category = match market {
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
        MarketKind::Spot => {
            return Err(AdapterError::InvalidRequest(
                "Mark-price history is unavailable for Bybit spot markets".to_string(),
            ));
        }
    };
    let interval = if period == Timeframe::D1 {
        "D".to_string()
    } else {
        period.to_minutes().to_string()
    };
    let mut url = format!(
        "{FETCH_DOMAIN}/v5/market/mark-price-kline?category={category}&symbol={}&interval={interval}",
        ticker_str.to_uppercase()
    );
    if let Some((start, end)) = range {
        let count =
            (end.as_u64().saturating_sub(start.as_u64()) / period.to_milliseconds()).clamp(1, 1000);
        url.push_str(&format!(
            "&start={}&end={}&limit={count}",
            start.as_u64(),
            end.as_u64()
        ));
    } else {
        url.push_str("&limit=200");
    }
    let response: ApiResponse = hub.http_json_with_limiter(&url, 1, None, None).await?;
    response
        .result
        .list
        .iter()
        .map(|row| {
            let time = parse_kline_field::<u64>(row.first().and_then(Value::as_str))?;
            let close = parse_kline_field::<f64>(row.get(4).and_then(Value::as_str))?;
            Ok((UnixMs::new(time), close))
        })
        .collect()
}

async fn fetch_current_oi(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
) -> Result<OpenInterest, AdapterError> {
    let (ticker_str, market) = ticker_info.ticker.to_full_symbol_and_type();
    let category = match market {
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
        MarketKind::Spot => {
            return Err(AdapterError::InvalidRequest(
                "Open interest is unavailable for Bybit spot markets".to_string(),
            ));
        }
    };
    let url = format!(
        "{FETCH_DOMAIN}/v5/market/tickers?category={category}&symbol={}",
        ticker_str.to_uppercase()
    );
    let content: Value = hub.http_json_with_limiter(&url, 1, None, None).await?;
    let time = serde_util::value_as_f64(&content["time"])
        .map(|value| value as u64)
        .ok_or_else(|| AdapterError::ParseError("Missing Bybit ticker time".to_string()))?;
    let item = content["result"]["list"]
        .as_array()
        .and_then(|list| list.first())
        .ok_or_else(|| AdapterError::ParseError("Missing Bybit ticker OI".to_string()))?;

    // Bybit now publishes both-side and single-side fields. Standard OI
    // counts each outstanding contract once, so prefer the single-side value;
    // halve the documented both-side field only as a compatibility fallback.
    let value = current_oi_usd_value(item, market)
        .ok_or_else(|| AdapterError::ParseError("Missing Bybit ticker OI value".to_string()))?;
    if !value.is_finite() || value < 0.0 {
        return Err(AdapterError::ParseError(format!(
            "Invalid current Bybit OI for {ticker_str}: {value}"
        )));
    }
    Ok(OpenInterest {
        time: UnixMs::new(time),
        value,
    })
}

fn current_oi_usd_value(item: &Value, market: MarketKind) -> Option<f64> {
    match market {
        MarketKind::LinearPerps => serde_util::value_as_f64(&item["singleOpenInterestValue"])
            .or_else(|| {
                serde_util::value_as_f64(&item["openInterestValue"]).map(|value| value / 2.0)
            })
            .or_else(|| {
                let base = serde_util::value_as_f64(&item["singleOpenInterest"]).or_else(|| {
                    serde_util::value_as_f64(&item["openInterest"]).map(|value| value / 2.0)
                })?;
                let mark = serde_util::value_as_f64(&item["markPrice"])?;
                Some(base * mark)
            }),
        MarketKind::InversePerps => serde_util::value_as_f64(&item["singleOpenInterest"])
            .or_else(|| serde_util::value_as_f64(&item["openInterest"]).map(|value| value / 2.0)),
        MarketKind::Spot => None,
    }
}

pub(super) async fn fetch_historical_oi(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
    range: Option<(UnixMs, UnixMs)>,
    period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    let ticker_str = ticker_info
        .ticker
        .to_full_symbol_and_type()
        .0
        .to_uppercase();
    let historical_period = match period {
        Timeframe::M1 | Timeframe::M3 => Timeframe::M5,
        other => other,
    };
    let period_str = match historical_period {
        Timeframe::M5 => "5min",
        Timeframe::M15 => "15min",
        Timeframe::M30 => "30min",
        Timeframe::H1 => "1h",
        Timeframe::H4 => "4h",
        Timeframe::D1 => "1d",
        _ => {
            return Err(AdapterError::InvalidRequest(format!(
                "Unsupported timeframe for open interest: {historical_period}"
            )));
        }
    };
    let market = ticker_info.market_type();
    let category = match market {
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
        MarketKind::Spot => {
            return Err(AdapterError::InvalidRequest(
                "Open interest is unavailable for Bybit spot markets".to_string(),
            ));
        }
    };

    let mut url = format!(
        "{FETCH_DOMAIN}/v5/market/open-interest?category={category}&symbol={ticker_str}&intervalTime={period_str}",
    );

    if let Some((start, end)) = range {
        let start = start.as_u64();
        let end = end.as_u64();
        let interval_ms = historical_period.to_milliseconds();
        let num_intervals = (end.saturating_sub(start) / interval_ms).clamp(1, 200);

        url.push_str(&format!(
            "&startTime={start}&endTime={end}&limit={num_intervals}"
        ));
    } else {
        url.push_str("&limit=200");
    }

    let response_text = hub.http_text_with_limiter(&url, 1, None, None).await?;

    let content: Value = sonic_rs::from_str(&response_text).map_err(|e| {
        log::error!(
            "Failed to parse JSON from {}: {}\nResponse: {}",
            url,
            e,
            response_text
        );
        AdapterError::ParseError(e.to_string())
    })?;

    let result_list = content["result"]["list"].as_array().ok_or_else(|| {
        log::error!("Result list is not an array in response: {}", response_text);
        AdapterError::ParseError("Result list is not an array".to_string())
    })?;

    let bybit_oi: Vec<DeOpenInterest> =
        serde_json::from_value(json!(result_list)).map_err(|e| {
            log::error!(
                "Failed to parse open interest array: {}\nResponse: {}",
                e,
                response_text
            );
            AdapterError::ParseError(format!("Failed to parse open interest: {e}"))
        })?;

    let prices = if market == MarketKind::LinearPerps {
        fetch_mark_price_closes(hub, ticker_info, historical_period, range).await?
    } else {
        std::collections::BTreeMap::new()
    };
    let mut open_interest: Vec<OpenInterest> = bybit_oi
        .into_iter()
        .filter_map(|x| {
            let time = UnixMs::from(x.timestamp);
            let contracts = x
                .single_side_value
                .as_deref()
                .and_then(|value| value.parse().ok())
                .unwrap_or(x.value / 2.0);
            let value = match market {
                MarketKind::LinearPerps => prices
                    .range(..=time)
                    .next_back()
                    .or_else(|| prices.first_key_value())
                    .map(|(_, price)| contracts * price)?,
                MarketKind::InversePerps => contracts,
                MarketKind::Spot => return None,
            };
            Some(OpenInterest { time, value })
        })
        .collect();

    let now = UnixMs::now();
    if oi_range_reaches_live_edge(range, period, now) {
        match fetch_current_oi(hub, ticker_info).await {
            Ok(snapshot) => open_interest.push(snapshot),
            Err(err) if open_interest.is_empty() => return Err(err),
            Err(err) => log::warn!(
                "Failed to refresh current Bybit OI for {ticker_str}; using native history: {err}"
            ),
        }
    }

    let mut deduplicated = BTreeMap::new();
    for point in open_interest {
        if point.value.is_finite() && point.value >= 0.0 {
            deduplicated.insert(point.time, point.value);
        }
    }
    let open_interest = deduplicated
        .into_iter()
        .map(|(time, value)| OpenInterest { time, value })
        .collect::<Vec<_>>();

    if open_interest.is_empty() {
        log::warn!(
            "No open interest data found for {}, from url: {}",
            ticker_str,
            url
        );
    }

    Ok(open_interest)
}

const MAX_ARCHIVE_TRADES_PER_FETCH: usize = 2_000_000;

async fn fetch_recent_trades(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
    from_time: UnixMs,
    to_time: Option<UnixMs>,
) -> Result<Vec<Trade>, AdapterError> {
    let (symbol, market_type) = ticker_info.ticker.to_full_symbol_and_type();
    let category = match market_type {
        MarketKind::Spot => "spot",
        MarketKind::LinearPerps => "linear",
        MarketKind::InversePerps => "inverse",
    };
    let limit = if market_type == MarketKind::Spot {
        60
    } else {
        1000
    };
    let url = format!(
        "{FETCH_DOMAIN}/v5/market/recent-trade?category={category}&symbol={}&limit={limit}",
        symbol.to_uppercase()
    );
    let response: Value = hub.http_json_with_limiter(&url, 1, None, None).await?;
    let list = response["result"]["list"]
        .as_array()
        .ok_or_else(|| AdapterError::ParseError("Bybit recent trades list is missing".into()))?;
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        volume_size_unit() == SizeUnit::Quote,
        ticker_info,
        raw_qty_unit_from_market_type(market_type),
    );
    let from_ms = from_time.as_u64();
    let to_ms = to_time.map(UnixMs::as_u64).unwrap_or(u64::MAX);
    let mut trades = list
        .iter()
        .filter_map(|item| {
            let time = item["time"].as_str()?.parse::<u64>().ok()?;
            if time < from_ms || time > to_ms {
                return None;
            }
            let price_f64 = item["price"].as_str()?.parse::<f64>().ok()?;
            let qty_f64 = item["size"].as_str()?.parse::<f64>().ok()?;
            Some(Trade {
                time: UnixMs::new(time),
                is_sell: item["side"].as_str()? == "Sell",
                price: Price::from_f64(price_f64).round_to_min_tick(ticker_info.min_ticksize),
                qty: qty_norm.normalize_qty(qty_f64, price_f64),
            })
        })
        .collect::<Vec<_>>();
    trades.sort_by_key(|trade| trade.time);
    Ok(trades)
}

async fn fetch_archive_trades(
    client: &reqwest::Client,
    ticker_info: TickerInfo,
    date: chrono::NaiveDate,
    from_time: UnixMs,
    to_time: Option<UnixMs>,
    data_path: PathBuf,
) -> Result<Vec<Trade>, AdapterError> {
    let (symbol, market_type) = ticker_info.ticker.to_full_symbol_and_type();
    if market_type == MarketKind::Spot {
        return Err(AdapterError::InvalidRequest(
            "Bybit spot trade archives are not supported yet".into(),
        ));
    }

    let symbol = symbol.to_uppercase();
    let filename = format!("{symbol}{}.csv.gz", date.format("%Y-%m-%d"));
    let cache_dir = data_path.join(&symbol);
    std::fs::create_dir_all(&cache_dir)
        .map_err(|err| AdapterError::ParseError(format!("Failed to create Bybit cache: {err}")))?;
    let archive_path = cache_dir.join(&filename);
    let missing_path = cache_dir.join(format!("{filename}.missing"));

    if missing_path.exists() {
        let marker_is_fresh = std::fs::metadata(&missing_path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < std::time::Duration::from_secs(12 * 60 * 60));
        if marker_is_fresh {
            return Ok(Vec::new());
        }
        let _ = std::fs::remove_file(&missing_path);
    }

    if !archive_path.exists() {
        let url = format!("https://public.bybit.com/trading/{symbol}/{filename}");
        log::info!("Downloading Bybit trade archive from {url}");
        let response = client
            .get(&url)
            .header(
                reqwest::header::USER_AGENT,
                concat!("flowsurface/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .map_err(AdapterError::from)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let _ = std::fs::write(&missing_path, b"");
            return Ok(Vec::new());
        }
        if !response.status().is_success() {
            return Err(AdapterError::InvalidRequest(format!(
                "Failed to fetch {url}: {}",
                response.status()
            )));
        }
        let body = response.bytes().await.map_err(AdapterError::from)?;
        std::fs::write(&archive_path, body).map_err(|err| {
            AdapterError::ParseError(format!("Failed to cache Bybit archive: {err}"))
        })?;
        let _ = std::fs::remove_file(&missing_path);
    }

    let file = std::fs::File::open(&archive_path)
        .map_err(|err| AdapterError::ParseError(format!("Failed to open Bybit archive: {err}")))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut csv = ReaderBuilder::new().has_headers(true).from_reader(decoder);
    let qty_norm = QtyNormalization::with_raw_qty_unit(
        volume_size_unit() == SizeUnit::Quote,
        ticker_info,
        raw_qty_unit_from_market_type(market_type),
    );
    let from_ms = from_time.as_u64();
    let to_ms = to_time.map(UnixMs::as_u64).unwrap_or(u64::MAX);
    let mut trades = Vec::with_capacity(MAX_ARCHIVE_TRADES_PER_FETCH);

    for record in csv.records() {
        let Ok(record) = record else { continue };
        let Some(timestamp) = record.get(0).and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        let time = if timestamp >= 1_000_000_000_000.0 {
            timestamp as u64
        } else {
            (timestamp * 1000.0).round() as u64
        };
        if time < from_ms {
            continue;
        }
        if time > to_ms {
            break;
        }
        let Some(side) = record.get(2) else { continue };
        let Some(qty_f64) = record.get(3).and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        let Some(price_f64) = record.get(4).and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        trades.push(Trade {
            time: UnixMs::new(time),
            is_sell: side == "Sell",
            price: Price::from_f64(price_f64).round_to_min_tick(ticker_info.min_ticksize),
            qty: qty_norm.normalize_qty(qty_f64, price_f64),
        });
        if trades.len() >= MAX_ARCHIVE_TRADES_PER_FETCH {
            break;
        }
    }
    trades.sort_by_key(|trade| trade.time);
    Ok(trades)
}

pub(super) async fn fetch_trades(
    hub: &mut HttpHub<BybitLimiter>,
    ticker_info: TickerInfo,
    from_time: UnixMs,
    to_time: Option<UnixMs>,
    data_path: Option<PathBuf>,
) -> Result<Vec<Trade>, AdapterError> {
    /// Archive days to download concurrently. Results are processed strictly
    /// in order, so paging semantics match a serial walk — the wall-clock win
    /// comes from overlapping the ~seconds-long archive downloads.
    const ARCHIVE_PREFETCH_DAYS: usize = 4;

    let from_ms = i64::try_from(from_time.as_u64())
        .map_err(|_| AdapterError::InvalidRequest("Timestamp exceeds i64 range".into()))?;
    let mut date = chrono::DateTime::from_timestamp_millis(from_ms)
        .ok_or_else(|| AdapterError::ParseError("Invalid Bybit archive timestamp".into()))?
        .date_naive();
    let today = chrono::Utc::now().date_naive();
    let data_path = data_path.ok_or_else(|| {
        AdapterError::InvalidRequest("Bybit trade archive fetch requires data_path".into())
    })?;
    let end_date = to_time
        .and_then(|time| i64::try_from(time.as_u64()).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.date_naive())
        .unwrap_or(today);
    let mut cursor = from_time;

    while date <= end_date {
        if date >= today {
            return fetch_recent_trades(hub, ticker_info, cursor, to_time).await;
        }

        let mut window: Vec<(chrono::NaiveDate, UnixMs)> = Vec::new();
        {
            let mut probe_date = date;
            let mut probe_cursor = cursor;
            while probe_date < today
                && probe_date <= end_date
                && window.len() < ARCHIVE_PREFETCH_DAYS
            {
                window.push((probe_date, probe_cursor));
                let Some(next_date) = probe_date.succ_opt() else {
                    break;
                };
                let Some(next_midnight) = next_date.and_hms_opt(0, 0, 0) else {
                    break;
                };
                let next_ms = next_midnight.and_utc().timestamp_millis();
                probe_cursor = UnixMs::new(u64::try_from(next_ms).map_err(|_| {
                    AdapterError::InvalidRequest(
                        "Bybit archive timestamp is before Unix epoch".into(),
                    )
                })?);
                probe_date = next_date;
            }
        }

        let client = hub.client().clone();
        let downloads = window.iter().map(|(window_date, window_from)| {
            let client = client.clone();
            let data_path = data_path.clone();
            async move {
                fetch_archive_trades(
                    &client,
                    ticker_info,
                    *window_date,
                    *window_from,
                    to_time,
                    data_path,
                )
                .await
            }
        });
        let results = futures::future::join_all(downloads).await;

        for ((window_date, _), result) in window.into_iter().zip(results) {
            let trades = result?;
            if !trades.is_empty() {
                return Ok(trades);
            }

            let Some(next_date) = window_date.succ_opt() else {
                return Ok(Vec::new());
            };
            let Some(next_midnight) = next_date.and_hms_opt(0, 0, 0) else {
                return Ok(Vec::new());
            };
            let next_ms = next_midnight.and_utc().timestamp_millis();
            date = next_date;
            cursor = UnixMs::new(u64::try_from(next_ms).map_err(|_| {
                AdapterError::InvalidRequest("Bybit archive timestamp is before Unix epoch".into())
            })?);
        }
    }

    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_linear_oi_prefers_single_side_usd_notional() {
        let item = serde_json::json!({
            "openInterest": "49185.575",
            "openInterestValue": "3803594293.11",
            "singleOpenInterest": "24592.788",
            "singleOpenInterestValue": "1901797185.22",
            "markPrice": "77329.5"
        });
        assert_eq!(
            current_oi_usd_value(&item, MarketKind::LinearPerps),
            Some(1_901_797_185.22)
        );
    }

    #[test]
    fn documented_both_side_value_is_halved_when_single_side_is_absent() {
        let linear = serde_json::json!({
            "openInterestValue": "3800000000",
            "markPrice": "77000"
        });
        assert_eq!(
            current_oi_usd_value(&linear, MarketKind::LinearPerps),
            Some(1_900_000_000.0)
        );

        let inverse = serde_json::json!({ "openInterest": "240000000" });
        assert_eq!(
            current_oi_usd_value(&inverse, MarketKind::InversePerps),
            Some(120_000_000.0)
        );
    }
}
