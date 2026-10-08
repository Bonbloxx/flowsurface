//! Export bounded, coverage-proven recorder history for the production replay.
//! No server writes, raw-trade cache, new protocol or new HTTP implementation.
//! Usage: cargo run --release --example orderflow_history -- URL FROM_MS TO_MS output.csv [coverage-only]
use exchange::{Ticker, TickerInfo, UnixMs, adapter::Exchange};
use std::{
    fs::File,
    io::{BufWriter, Write},
    time::Duration,
};

#[allow(dead_code)]
mod client {
    include!("../src/connector/client.rs");
    pub fn replay_client(url: &str) -> Option<ServerClient> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        ServerClient::new(url, data::config::auth::load_server_token(url), http)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).ok_or("recorder URL required")?;
    let from = UnixMs::new(args.get(2).ok_or("FROM_MS required")?.parse()?);
    let to = UnixMs::new(args.get(3).ok_or("TO_MS required")?.parse()?);
    if from >= to || to.as_u64() - from.as_u64() > 24 * 3_600_000 {
        return Err("export must be an increasing range of at most one day".into());
    }
    let output = args.get(4).ok_or("output.csv required")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = client::replay_client(url).ok_or("could not create recorder client")?;
        let ticker = TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let coverage = client.trade_coverage(ticker, from, to).await?;
        let missing = coverage.missing_ranges(from, to);
        eprintln!("coverage {:?}; missing {:?}", coverage.segments, missing);
        if args.get(5).is_some_and(|v| v == "coverage-only") {
            return Ok(());
        }
        if !missing.is_empty() {
            return Err("recorder range is incomplete; export aborted".into());
        }
        let mut writer = BufWriter::new(File::create(output)?);
        let mut cursor = from;
        let mut count = 0usize;
        let mut pages = 0usize;
        let mut page_limit = 50_000;
        while cursor <= to {
            if pages >= 1_000 {
                return Err("bounded page limit exceeded".into());
            }
            let page = client
                .fetch_trades_arrow(ticker, cursor, to, page_limit)
                .await?;
            pages += 1;
            if page.raw_row_count == 0 {
                break;
            }
            let last = page.last_ts.ok_or("missing page cursor")?;
            // Re-fetch a capped page's entire final millisecond. A ts+1 cursor
            // would drop timestamp ties at the limit boundary.
            let capped = page.raw_row_count == page_limit;
            let keep = if capped {
                page.trades.partition_point(|t| t.time < last)
            } else {
                page.trades.len()
            };
            if keep == 0 && capped {
                if page_limit >= 400_000 {
                    return Err("oversized trade millisecond".into());
                }
                page_limit = (page_limit * 2).min(400_000);
                continue;
            }
            for trade in &page.trades[..keep] {
                count += 1;
                if count > 2_000_000 {
                    return Err("export exceeds the two-million execution cap".into());
                }
                writeln!(
                    writer,
                    "{},{},{},0,0,{},{}",
                    count,
                    trade.price.to_f64(),
                    trade.qty.to_f64(),
                    trade.time,
                    trade.is_sell
                )?;
            }
            writer.flush()?;
            if writer.get_ref().metadata()?.len() > 128 * 1024 * 1024 {
                return Err("export exceeds the 128 MiB local disk cap".into());
            }
            cursor = if capped { last } else { last.saturating_add(1) };
            if !capped {
                break;
            }
            page_limit = 50_000;
            eprintln!("page {pages}: {count} executions exported");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        writer.flush()?;
        eprintln!("complete: {count} executions, {pages} sequential pages");
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
