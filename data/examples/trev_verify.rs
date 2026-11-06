//! Exact replay/export and bounded performance probe for the production TRev engine.
//! cargo run --release -p flowsurface-data --example trev_verify -- CSV OUTPUT_DIR
use exchange::{
    Ticker, Trade, UnixMs,
    adapter::Exchange,
    unit::{MinTicksize, Price, PriceStep, Qty},
};
use flowsurface_data::{aggr::ticks::TickAggr, chart::trev::Config};
use serde_json::json;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    time::Instant,
};

fn stats(mut samples: Vec<u64>) -> serde_json::Value {
    samples.sort_unstable();
    json!({"samples":samples.len(),"p50_us":samples[samples.len()/2] as f64/1000.,
        "p95_us":samples[samples.len()*95/100] as f64/1000.,
        "p99_us":samples[samples.len()*99/100] as f64/1000.,
        "max_us":samples.last().copied().unwrap() as f64/1000.})
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let input = args.get(1).ok_or("CSV required")?;
    let output = PathBuf::from(args.get(2).ok_or("OUTPUT_DIR required")?);
    fs::create_dir_all(&output)?;
    if File::open(input)?.metadata()?.len() > 128 * 1024 * 1024 {
        return Err("input exceeds 128 MiB".into());
    }
    let mut trades = Vec::new();
    let minimum = MinTicksize::from(0.1);
    let step = PriceStep::from(minimum);
    for line in BufReader::new(File::open(input)?).lines() {
        let line = line?;
        let fields: Vec<_> = line.trim().split(',').collect();
        if fields.len() < 7 {
            return Err("invalid execution record".into());
        }
        trades.push(Trade {
            time: UnixMs::new(fields[5].parse()?),
            price: Price::from_f64(fields[1].parse()?).round_to_min_tick(minimum),
            qty: Qty::from_f64(fields[2].parse()?),
            is_sell: fields[6].parse()?,
        });
        if trades.len() > 2_000_000 {
            return Err("input exceeds two million executions".into());
        }
    }
    if trades.is_empty() || !trades.windows(2).all(|w| w[0].time <= w[1].time) {
        return Err("ordered, nonempty tape required".into());
    }
    let ticker = Ticker::new("BTCUSDT", Exchange::BinanceLinear);
    let mut cases = Vec::new();
    for (name, config) in [
        ("default", Config::default()),
        (
            "legacy-ratio",
            Config {
                continuation_ticks: 420,
                reversal_ticks: 260,
                normalization_ms: 250,
                ..Default::default()
            },
        ),
        (
            "small",
            Config {
                continuation_ticks: 10,
                reversal_ticks: 15,
                ..Default::default()
            },
        ),
    ] {
        let start = Instant::now();
        let full = TickAggr::new_trev(config, step, &trades);
        let cold_ms = start.elapsed().as_secs_f64() * 1000.;
        let before = full.trev_snapshot(ticker).unwrap();
        let snapshot_start = Instant::now();
        let bytes = serde_json::to_vec(&before)?;
        let serialize_ms = snapshot_start.elapsed().as_secs_f64() * 1000.;
        fs::write(output.join(format!("{name}.json")), &bytes)?;
        let mut chunked = TickAggr::new_trev(config, step, &[]);
        let mut samples = Vec::new();
        for batch in trades.chunks(32) {
            let start = Instant::now();
            chunked.insert_trades(batch);
            samples.push(start.elapsed().as_nanos() as u64);
        }
        assert_eq!(
            serde_json::to_vec(&chunked.trev_snapshot(ticker))?,
            serde_json::to_vec(&Some(before))?,
            "chunking changed data"
        );
        cases.push(json!({"case":name,"cold_ms":cold_ms,"snapshot_bytes":bytes.len(),"serialize_ms":serialize_ms,
            "bars":full.datapoints.len(),"rows":full.datapoints.iter().map(|b|b.footprint.trades.len()).sum::<usize>(),"batch32":stats(samples)}));
    }
    let wide_config = Config {
        continuation_ticks: 100_000,
        reversal_ticks: 100_000,
        ..Default::default()
    };
    let wide = (0..20_000)
        .map(|i| Trade {
            time: UnixMs::new(i),
            price: Price::from_units(Price::from_f64(100_000.).units + i as i64 * step.units),
            qty: Qty::from_f64(1.),
            is_sell: i % 2 == 0,
        })
        .collect::<Vec<_>>();
    let mut wide_aggr = TickAggr::new_trev(wide_config, step, &wide);
    wide_aggr.set_trev_display(flowsurface_data::chart::trev::Display::BidAsk);
    let mut wide_samples = Vec::new();
    for i in 0..4_000 {
        let trade = Trade {
            time: UnixMs::new(20_000 + i),
            ..wide[i as usize % wide.len()]
        };
        let start = Instant::now();
        wide_aggr.insert_trades(&[trade]);
        wide_samples.push(start.elapsed().as_nanos() as u64);
    }
    let mut stress = TickAggr::new_trev(Config::default(), step, &[]);
    let stress_start = Instant::now();
    let mut batch = Vec::with_capacity(1000);
    for i in 0..1_000_000_u64 {
        batch.push(Trade {
            time: UnixMs::new(i * 10),
            price: Price::from_f64(100_000. + (i % 8000) as f64 * 0.1),
            qty: Qty::from_f64(0.1),
            is_sell: i % 2 == 0,
        });
        if batch.len() == 1000 {
            stress.insert_trades(&batch);
            batch.clear();
        }
    }
    let report = json!({"input_executions":trades.len(),"first_ms":trades[0].time,"last_ms":trades.last().unwrap().time,
        "cases":cases,"wide_20k_rows_single_execution":stats(wide_samples),
        "stress":{"executions":1_000_000,"elapsed_ms":stress_start.elapsed().as_secs_f64()*1000.,"bars":stress.datapoints.len(),"rows":stress.datapoints.iter().map(|b|b.footprint.trades.len()).sum::<usize>()}});
    fs::write(
        output.join("performance.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
