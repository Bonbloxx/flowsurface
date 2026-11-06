//! Bounded resource probe for the production orderflow summary book.
//! Usage: orderflow_resources CSV HOURS [config.json]
use exchange::{
    Trade, UnixMs,
    unit::{MinTicksize, Price, Qty},
};
use flowsurface_data::chart::orderflow;
use orderflow::{Config, Detector, Tape};
use serde_json::json;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let file = args.get(1).ok_or("CSV required")?;
    let hours: u64 = args.get(2).ok_or("HOURS required")?.parse()?;
    if !(1..=24).contains(&hours) || File::open(file)?.metadata()?.len() > 128 * 1024 * 1024 {
        return Err("probe accepts at most 24 hours and a 128 MiB input".into());
    }
    let config: Config = if let Some(path) = args.get(3) {
        serde_json::from_reader(File::open(path)?)?
    } else {
        Config::default()
    };
    let retention = hours * 3_600_000 + 600_000;
    let step = Detector::new(config, MinTicksize::from(0.1).into()).step();
    let mut tape = Tape::default();
    let mut batch = Vec::with_capacity(25_000);
    let mut reader = BufReader::new(File::open(file)?);
    let mut line = String::new();
    let mut first = None;
    let mut last = 0;
    let mut ingestion_us = Vec::new();
    let mut publish = |tape: &mut Tape, batch: &mut Vec<Trade>| {
        let start = Instant::now();
        tape.insert(batch, step, false);
        if let Some((&latest, _)) = tape.seconds.last_key_value() {
            let oldest = latest.saturating_sub(retention);
            while tape
                .seconds
                .first_key_value()
                .is_some_and(|(&t, _)| t < oldest)
            {
                tape.seconds.pop_first();
            }
        }
        ingestion_us.push(start.elapsed().as_micros() as u64);
        batch.clear();
    };
    while reader.read_line(&mut line)? != 0 {
        let fields: Vec<_> = line.trim().split(',').collect();
        if fields.len() >= 7 && fields[0].parse::<u64>().is_ok() {
            let mut ms: u64 = fields[5].parse()?;
            if ms > 10_000_000_000_000 {
                ms /= 1_000;
            }
            if ms < last {
                return Err("CSV is not ordered".into());
            }
            first.get_or_insert(ms);
            last = ms;
            batch.push(Trade {
                time: UnixMs::new(ms),
                price: Price::from_f64(fields[1].parse()?),
                qty: Qty::from_f64(fields[2].parse()?),
                is_sell: fields[6].eq_ignore_ascii_case("true"),
            });
            if batch.len() == 25_000 {
                publish(&mut tape, &mut batch);
            }
            if tape.prints + batch.len() as u64 > 2_000_000 {
                return Err("two-million execution cap exceeded".into());
            }
        }
        line.clear();
    }
    publish(&mut tape, &mut batch);
    drop(reader);
    drop(batch);
    ingestion_us.sort_unstable();
    let mut rebuild_ms = Vec::new();
    let mut hot_ns = Vec::new();
    let mut marks = 0;
    let mut gap_resets = 0;
    for iteration in 0..5 {
        let start = Instant::now();
        let mut detector = Detector::new(config, MinTicksize::from(0.1).into());
        for second in tape.seconds.values() {
            let before = Instant::now();
            detector.process(second);
            if iteration == 0 && second.time >= last.saturating_sub(600_000) {
                hot_ns.push(before.elapsed().as_nanos() as u64);
            }
        }
        rebuild_ms.push(start.elapsed().as_secs_f64() * 1_000.0);
        marks = detector.events().len();
        gap_resets = detector.gap_resets;
    }
    hot_ns.sort_unstable();
    let observed_bytes = tape.estimated_bytes();
    let worst_case_bytes = (retention / 1_000 + 1) as usize
        * (std::mem::size_of::<orderflow::Second>()
            + 64
            + orderflow::MAX_LEVELS_PER_SECOND * std::mem::size_of::<(Price, orderflow::Sides)>());
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "input":file,"hours":hours,"config":config,"prints":tape.prints,
            "input_from_ms":first,"input_to_ms":last,"retained_seconds":tape.seconds.len(),
            "retained_estimated_bytes":observed_bytes,"bounded_worst_case_estimated_bytes":worst_case_bytes,
            "rebuild_ms":rebuild_ms,"marks":marks,"gap_resets":gap_resets,
            "ingest_25k_us_p50":ingestion_us[ingestion_us.len()/2],
            "ingest_25k_us_max":ingestion_us.last(),
            "live_second_ns_p50":hot_ns.get(hot_ns.len()/2),
            "live_second_ns_p99":hot_ns.get(hot_ns.len()*99/100),
            "raw_trades_retained":0
        }))?
    );
    // Keep the real summary allocations alive long enough for an external
    // working-set/peak-working-set observer; this is never used by the app.
    std::thread::sleep(Duration::from_millis(500));
    std::hint::black_box(tape);
    Ok(())
}
