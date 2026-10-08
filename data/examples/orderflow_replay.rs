//! Replay an immutable Binance aggTrades CSV with the production detector.
//! Usage: cargo run -p flowsurface-data --release --example orderflow_replay -- file.csv [config.json]
//! Outcomes start AFTER confirmation plus the live two-second publication buffer, not at the earlier extreme. This is a
//! diagnostic event study; fees/slippage and portfolio execution are not modeled.
use exchange::{
    Trade, UnixMs,
    unit::{Price, PriceStep, Qty},
};
use flowsurface_data::chart::orderflow::{Config, Detector, Event, Second, Sides, Tape};
use serde_json::json;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

fn outcome(
    seconds: &[Second],
    time: u64,
    price: f64,
    bullish: bool,
    risk: f64,
) -> Option<(bool, f64, f64)> {
    if seconds.last()?.time < time + 300_000 {
        return None;
    }
    let start = seconds.partition_point(|second| second.time <= time);
    let end = seconds.partition_point(|second| second.time <= time + 300_000);
    let mut mfe: f64 = 0.0;
    let mut mae: f64 = 0.0;
    let mut hit = None;
    for second in &seconds[start..end] {
        let (favorable, adverse) = if bullish {
            (second.high.to_f64() - price, price - second.low.to_f64())
        } else {
            (price - second.low.to_f64(), second.high.to_f64() - price)
        };
        mfe = mfe.max(favorable / risk);
        mae = mae.max(adverse / risk);
        // Conservative if both barriers fall within one second.
        if hit.is_none() {
            if adverse >= risk {
                hit = Some(false);
            } else if favorable >= risk {
                hit = Some(true);
            }
        }
    }
    Some((hit.unwrap_or(false), mfe, mae))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let config = if let Some(path) = args.get(2) {
        serde_json::from_reader(File::open(path)?)?
    } else {
        Config::default()
    };
    let mut detector = Detector::new(
        config,
        PriceStep::from(exchange::unit::MinTicksize::from(0.1)),
    );
    let mut tape = Tape::default();
    let mut seconds = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    let mut current = None;
    let mut count = 0u64;
    let mut last_ms = 0;
    let mut line = String::new();
    let mut reader = BufReader::new(File::open(args.get(1).ok_or("CSV required")?)?);
    let started = Instant::now();
    let mut detect_time = Duration::ZERO;
    let mut process = |second: &Second, detector: &mut Detector| {
        let before = Instant::now();
        detector.process(second);
        detect_time += before.elapsed();
        events.extend(
            detector
                .events()
                .iter()
                .filter(|event| event.confirmed == Some(second.time + 999))
                .cloned(),
        );
        seconds.push(second.clone());
    };
    while reader.read_line(&mut line)? != 0 {
        let fields: Vec<&str> = line.trim().split(',').collect();
        if fields.len() >= 7 && fields[0].parse::<u64>().is_ok() {
            let mut ms: u64 = fields[5].parse()?;
            if ms > 10_000_000_000_000 {
                ms /= 1_000;
            }
            if ms < last_ms {
                return Err("CSV not ordered".into());
            }
            last_ms = ms;
            let time = ms / 1_000 * 1_000;
            if current.is_some_and(|old| old != time) {
                for second in tape.seconds.values() {
                    process(second, &mut detector);
                }
                tape.seconds.clear();
            }
            current = Some(time);
            tape.insert(
                &[Trade {
                    time: UnixMs::new(ms),
                    price: Price::from_f64(fields[1].parse()?),
                    qty: Qty::from_f64(fields[2].parse()?),
                    is_sell: fields[6].eq_ignore_ascii_case("true"),
                }],
                detector.step(),
                false,
            );
            count += 1;
        }
        line.clear();
    }
    for second in tape.seconds.values() {
        process(second, &mut detector);
    }
    let mut samples = Vec::new();
    let mut wins = 0;
    let mut baseline_wins = 0;
    let mut eligible = 0;
    let mut invalid_before_entry = 0;
    let first = seconds.first().ok_or("empty archive")?.time;
    let span = seconds.last().unwrap().time - first;
    for event in &events {
        let confirmed = event.confirmed.unwrap();
        let index = seconds.partition_point(|second| second.time + 999 < confirmed + 2_000);
        let Some(entry_second) = seconds.get(index) else {
            continue;
        };
        let time = entry_second.time + 999;
        let entry = entry_second.last.to_f64();
        let risk = if event.bullish {
            entry - event.price.to_f64()
        } else {
            event.price.to_f64() - entry
        } + detector.step().to_f64_lossy();
        if risk <= 0.0 {
            invalid_before_entry += 1;
            continue;
        }
        if let Some((hit, mfe, mae)) = outcome(&seconds, time, entry, event.bullish, risk) {
            eligible += 1;
            wins += usize::from(hit);
            // Time-shifted control, retaining direction and identical dollar risk.
            let shifted =
                first + ((time - first + 1_200_000) % span.saturating_sub(300_000).max(1));
            let index = seconds
                .partition_point(|second| second.time < shifted)
                .min(seconds.len() - 1);
            let control = &seconds[index];
            if outcome(
                &seconds,
                control.time,
                control.last.to_f64(),
                event.bullish,
                risk,
            )
            .is_some_and(|o| o.0)
            {
                baseline_wins += 1;
            }
            samples.push(json!({"kind":format!("{:?}",event.kind), "bullish":event.bullish, "observed":event.observed,
                "confirmed":confirmed, "entry_time":time, "anchor":event.price.to_f64(), "entry":entry, "risk_usd":risk,
                "target_1r_before_stop_5m":hit, "mfe_r_5m":mfe, "mae_r_5m":mae}));
        }
    }
    let compact_bytes: usize = seconds
        .iter()
        .rev()
        .take(15_001)
        .map(|s| {
            std::mem::size_of::<Second>()
                + 64
                + s.levels.capacity() * std::mem::size_of::<(Price, Sides)>()
        })
        .sum();
    let report = json!({"input":args[1], "config":config, "prints":count,"seconds":seconds.len(),"hours":span as f64/3_600_000.0,
        "parse_aggregate_detect_ms":started.elapsed().as_millis(),"detector_ms":detect_time.as_millis(),
        "retained_4h10m_estimated_bytes":compact_bytes,"gap_resets":detector.gap_resets,
        "signals":events.len(),"eligible_5m":eligible,"invalid_before_entry":invalid_before_entry,"publication_buffer_ms":2_000,"target_1r_before_stop_5m":wins,"shifted_control_wins":baseline_wins,
        "samples":samples});
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
