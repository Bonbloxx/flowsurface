use exchange::{OpenInterest, Ticker, TickerInfo, UnixMs};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

// v3 invalidates Bybit values cached before the venue-published both-side OI
// correction. Older cache directories remain untouched and are never read.
const CACHE_SCHEMA_VERSION: u16 = 3;
const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 24 * 60 * MINUTE_MS;
const RETENTION_DAYS: u64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedOpenInterestDay {
    schema_version: u16,
    source: String,
    day_start: u64,
    /// One exact venue observation per minute. Repeated observations in the
    /// same minute replace that minute's close rather than creating unbounded
    /// duplicate rows.
    points: BTreeMap<u64, CachedObservation>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
struct CachedObservation {
    observed_at: u64,
    value: f64,
}

fn day_start(time: u64) -> u64 {
    time / DAY_MS * DAY_MS
}

fn minute_start(time: u64) -> u64 {
    time / MINUTE_MS * MINUTE_MS
}

fn oldest_retained_day(now: UnixMs) -> u64 {
    day_start(now.as_u64()).saturating_sub((RETENTION_DAYS - 1) * DAY_MS)
}

fn safe_source_identity(source: TickerInfo) -> String {
    let identity = source.ticker.symbol_and_exchange_string();
    let mut encoded = String::with_capacity(identity.len() * 2);
    for byte in identity.bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn cache_root() -> PathBuf {
    let relative = format!("market_data/open-interest/v{CACHE_SCHEMA_VERSION}");
    if let Ok(override_path) = std::env::var("FLOWSURFACE_DATA_PATH") {
        let override_path = PathBuf::from(override_path);
        let base = if override_path.is_dir() || override_path.extension().is_none() {
            override_path
        } else {
            override_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        base.join(relative)
    } else {
        data::data_path(Some(&relative))
    }
}

fn source_cache_dir(root: &Path, source: TickerInfo) -> PathBuf {
    root.join(safe_source_identity(source))
}

fn cache_path(root: &Path, source: TickerInfo, day: u64) -> PathBuf {
    source_cache_dir(root, source).join(format!("{day}.oibin"))
}

fn read_cached_day(
    path: &Path,
    source: TickerInfo,
    day: u64,
) -> Option<BTreeMap<u64, CachedObservation>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            log::warn!("Failed to read OI cache {path:?}: {err}");
            return None;
        }
    };
    let (cached, _) = match bincode::serde::decode_from_slice::<CachedOpenInterestDay, _>(
        &bytes,
        bincode::config::standard(),
    ) {
        Ok(decoded) => decoded,
        Err(err) => {
            log::warn!("Ignoring corrupt OI cache {path:?}: {err}");
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    let valid_source = Ticker::parse_symbol_and_exchange(&cached.source)
        .is_some_and(|ticker| ticker.same_market(&source.ticker));
    let day_end = day.saturating_add(DAY_MS);
    let valid_points = cached.points.iter().all(|(minute, observation)| {
        *minute >= day
            && *minute < day_end
            && *minute == minute_start(*minute)
            && minute_start(observation.observed_at) == *minute
            && observation.value.is_finite()
            && observation.value >= 0.0
    });
    if cached.schema_version != CACHE_SCHEMA_VERSION
        || !valid_source
        || cached.day_start != day
        || !valid_points
    {
        log::warn!("Ignoring mismatched OI cache {path:?}");
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(cached.points)
}

fn write_cached_day(
    path: &Path,
    source: TickerInfo,
    day: u64,
    updates: &BTreeMap<u64, CachedObservation>,
) -> std::io::Result<()> {
    static WRITE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut points = read_cached_day(path, source, day).unwrap_or_default();
    let mut changed = false;
    for (time, observation) in updates {
        let should_replace = points
            .get(time)
            .is_none_or(|current| observation.observed_at >= current.observed_at);
        if should_replace && points.get(time) != Some(observation) {
            points.insert(*time, *observation);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }

    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "OI cache path has no parent",
        ));
    };
    std::fs::create_dir_all(parent)?;
    let payload = CachedOpenInterestDay {
        schema_version: CACHE_SCHEMA_VERSION,
        source: source.ticker.symbol_and_exchange_string(),
        day_start: day,
        points,
    };
    let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard())
        .map_err(std::io::Error::other)?;
    let temp_path = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp_path, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let target_won_race = path.exists();
        let _ = std::fs::remove_file(&temp_path);
        if !target_won_race {
            return Err(err);
        }
    }
    Ok(())
}

fn cleanup_stale_days_once(root: &Path, source: TickerInfo, now: UnixMs) {
    static CLEANED: OnceLock<Mutex<HashSet<Ticker>>> = OnceLock::new();
    let cleaned = CLEANED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut cleaned = cleaned
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !cleaned.insert(source.ticker) {
        return;
    }
    drop(cleaned);

    let oldest = oldest_retained_day(now);
    let dir = source_cache_dir(root, source);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let stale = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u64>().ok())
            .is_some_and(|day| day < oldest);
        if stale && let Err(err) = std::fs::remove_file(&path) {
            log::warn!("Failed to remove stale OI cache {path:?}: {err}");
        }
    }
}

fn load_range_from_root(
    root: &Path,
    source: TickerInfo,
    range: Option<(UnixMs, UnixMs)>,
    now: UnixMs,
) -> Vec<OpenInterest> {
    let oldest = oldest_retained_day(now);
    let (from, to) = range.unwrap_or((UnixMs::new(oldest), now));
    let from = from.as_u64().max(oldest);
    let to = to.as_u64().min(now.as_u64().saturating_add(MINUTE_MS));
    if from > to {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut day = day_start(from);
    let end_day = day_start(to);
    while day <= end_day {
        let path = cache_path(root, source, day);
        if let Some(points) = read_cached_day(&path, source, day) {
            result.extend(points.into_iter().filter_map(|(minute, observation)| {
                (minute >= from && minute <= to).then_some(OpenInterest {
                    time: UnixMs::new(observation.observed_at),
                    value: observation.value,
                })
            }));
        }
        let next = day.saturating_add(DAY_MS);
        if next <= day {
            break;
        }
        day = next;
    }
    result.sort_by_key(|point| point.time);
    result
}

pub(super) fn load_range(source: TickerInfo, range: Option<(UnixMs, UnixMs)>) -> Vec<OpenInterest> {
    let root = cache_root();
    let now = UnixMs::now();
    cleanup_stale_days_once(&root, source, now);
    load_range_from_root(&root, source, range, now)
}

pub(super) fn merge_and_load(
    source: TickerInfo,
    values: &[OpenInterest],
    range: Option<(UnixMs, UnixMs)>,
) -> Vec<OpenInterest> {
    let root = cache_root();
    let now = UnixMs::now();
    cleanup_stale_days_once(&root, source, now);
    merge_and_load_from_root(&root, source, values, range, now)
}

fn merge_and_load_from_root(
    root: &Path,
    source: TickerInfo,
    values: &[OpenInterest],
    range: Option<(UnixMs, UnixMs)>,
    now: UnixMs,
) -> Vec<OpenInterest> {
    let oldest = oldest_retained_day(now);

    let mut valid_values = values
        .iter()
        .copied()
        .filter(|point| point.value.is_finite() && point.value >= 0.0)
        .collect::<Vec<_>>();
    valid_values.sort_by_key(|point| point.time);

    let mut by_day = BTreeMap::<u64, BTreeMap<u64, CachedObservation>>::new();
    for point in valid_values
        .iter()
        .filter(|point| point.time.as_u64() >= oldest)
    {
        let minute = minute_start(point.time.as_u64());
        by_day.entry(day_start(minute)).or_default().insert(
            minute,
            CachedObservation {
                observed_at: point.time.as_u64(),
                value: point.value,
            },
        );
    }
    for (day, updates) in by_day {
        let path = cache_path(root, source, day);
        if let Err(err) = write_cached_day(&path, source, day, &updates) {
            log::warn!("Failed to persist OI cache {path:?}: {err}");
        }
    }

    let cached = load_range_from_root(root, source, range, now);
    let (from, to) = range
        .map(|(from, to)| (from.as_u64(), to.as_u64()))
        .unwrap_or((0, now.as_u64().saturating_add(MINUTE_MS)));
    let mut merged = BTreeMap::<u64, OpenInterest>::new();
    for point in cached.into_iter().chain(valid_values) {
        let minute = minute_start(point.time.as_u64());
        if minute < from || minute > to {
            continue;
        }
        let replace = merged
            .get(&minute)
            .is_none_or(|current| point.time >= current.time);
        if replace {
            merged.insert(minute, point);
        }
    }
    merged.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;

    fn source() -> TickerInfo {
        TickerInfo::new(
            Ticker::new("BTCUSDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        )
    }

    #[test]
    fn cache_keeps_one_exact_close_per_minute_and_round_trips() {
        let root = std::env::temp_dir().join(format!("flowsurface-oi-{}", uuid::Uuid::new_v4()));
        let source = source();
        let now = UnixMs::new(10 * DAY_MS + 90_000);
        let day = day_start(now.as_u64());
        let path = cache_path(&root, source, day);
        let updates = BTreeMap::from([
            (
                day + MINUTE_MS,
                CachedObservation {
                    observed_at: day + MINUTE_MS + 1_000,
                    value: 100.0,
                },
            ),
            (
                day + 2 * MINUTE_MS,
                CachedObservation {
                    observed_at: day + 2 * MINUTE_MS + 1_000,
                    value: 110.0,
                },
            ),
        ]);
        write_cached_day(&path, source, day, &updates).unwrap();
        write_cached_day(
            &path,
            source,
            day,
            &BTreeMap::from([(
                day + MINUTE_MS,
                CachedObservation {
                    observed_at: day + MINUTE_MS + 2_000,
                    value: 105.0,
                },
            )]),
        )
        .unwrap();

        let loaded = load_range_from_root(
            &root,
            source,
            Some((UnixMs::new(day), UnixMs::new(day + 3 * MINUTE_MS))),
            now,
        );
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].value, 105.0);
        assert_eq!(loaded[1].value, 110.0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_values_are_not_persisted() {
        let now = UnixMs::now();
        let values = [
            OpenInterest {
                time: now,
                value: f64::NAN,
            },
            OpenInterest {
                time: now,
                value: -1.0,
            },
        ];
        let oldest = oldest_retained_day(now);
        let mut sorted = values
            .iter()
            .copied()
            .filter(|point| {
                point.time.as_u64() >= oldest && point.value.is_finite() && point.value >= 0.0
            })
            .collect::<Vec<_>>();
        sorted.sort_by_key(|point| point.time);
        assert!(sorted.is_empty());
    }

    #[test]
    fn deep_remote_history_is_returned_but_not_added_to_bounded_cache() {
        let root = std::env::temp_dir().join(format!("flowsurface-oi-{}", uuid::Uuid::new_v4()));
        let source = source();
        let now = UnixMs::new(200 * DAY_MS);
        let old_time = UnixMs::new(10 * DAY_MS + 1_000);
        let values = [OpenInterest {
            time: old_time,
            value: 42.0,
        }];

        let merged = merge_and_load_from_root(
            &root,
            source,
            &values,
            Some((UnixMs::new(9 * DAY_MS), UnixMs::new(11 * DAY_MS))),
            now,
        );

        assert_eq!(merged, values);
        assert!(!cache_path(&root, source, day_start(old_time.as_u64())).exists());
    }

    #[test]
    fn older_exchange_observation_cannot_replace_newer_cached_observation() {
        let root = std::env::temp_dir().join(format!("flowsurface-oi-{}", uuid::Uuid::new_v4()));
        let source = source();
        let now = UnixMs::new(100 * DAY_MS + 5 * MINUTE_MS);
        let minute = minute_start(now.as_u64()).saturating_sub(MINUTE_MS);
        let range = Some((UnixMs::new(minute), UnixMs::new(minute + MINUTE_MS)));

        let newer = OpenInterest {
            time: UnixMs::new(minute + 40_000),
            value: 200.0,
        };
        let older = OpenInterest {
            time: UnixMs::new(minute + 5_000),
            value: 100.0,
        };
        let first = merge_and_load_from_root(&root, source, &[newer], range, now);
        let second = merge_and_load_from_root(&root, source, &[older], range, now);

        assert_eq!(first, vec![newer]);
        assert_eq!(second, vec![newer]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn source_cache_identity_is_path_safe_and_collision_free_for_punctuation() {
        let colon = source();
        let slash = TickerInfo::new(
            Ticker::new("BTC/USDT", Exchange::BinanceLinear),
            0.1,
            0.001,
            None,
        );
        let colon_identity = safe_source_identity(colon);
        let slash_identity = safe_source_identity(slash);
        assert!(colon_identity.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(slash_identity.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(colon_identity, slash_identity);
    }
}
