use exchange::{
    Kline, Timeframe, Trade, UnixMs,
    unit::price::{Price, PriceStep},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum ProfilePeriod {
    Hour,
    Hours2,
    Hours4,
    Hours8,
    Hours12,
    #[default]
    Day,
    Week,
}

impl ProfilePeriod {
    pub const ALL: [Self; 7] = [
        Self::Hour,
        Self::Hours2,
        Self::Hours4,
        Self::Hours8,
        Self::Hours12,
        Self::Day,
        Self::Week,
    ];

    pub const fn minutes(self) -> u64 {
        match self {
            Self::Hour => 60,
            Self::Hours2 => 120,
            Self::Hours4 => 240,
            Self::Hours8 => 480,
            Self::Hours12 => 720,
            Self::Day => 1_440,
            Self::Week => 10_080,
        }
    }

    pub const fn millis(self) -> u64 {
        self.minutes() * 60_000
    }
}

impl std::fmt::Display for ProfilePeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hour => write!(f, "1 hour"),
            Self::Hours2 => write!(f, "2 hours"),
            Self::Hours4 => write!(f, "4 hours"),
            Self::Hours8 => write!(f, "8 hours"),
            Self::Hours12 => write!(f, "12 hours"),
            Self::Day => write!(f, "1 day"),
            Self::Week => write!(f, "1 week"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum BlockSize {
    Minutes5,
    Minutes10,
    Minutes15,
    #[default]
    Minutes30,
    Hour,
    Hours2,
    Hours4,
}

impl BlockSize {
    pub const ALL: [Self; 7] = [
        Self::Minutes5,
        Self::Minutes10,
        Self::Minutes15,
        Self::Minutes30,
        Self::Hour,
        Self::Hours2,
        Self::Hours4,
    ];

    pub const fn minutes(self) -> u64 {
        match self {
            Self::Minutes5 => 5,
            Self::Minutes10 => 10,
            Self::Minutes15 => 15,
            Self::Minutes30 => 30,
            Self::Hour => 60,
            Self::Hours2 => 120,
            Self::Hours4 => 240,
        }
    }

    pub const fn millis(self) -> u64 {
        self.minutes() * 60_000
    }

    /// Underlying bar timeframe used to build letter high–low ranges.
    ///
    /// Sierra Chart builds letters from 1-minute bars when the letter period is
    /// in minutes. We match the letter period when the exchange supports that
    /// candle size (standard Market Profile path: one bar ≈ one letter), and
    /// fall back to a finer supported bar otherwise (e.g. 10m letters → 5m bars).
    pub const fn letter_timeframe(self) -> Timeframe {
        match self {
            Self::Minutes5 => Timeframe::M5,
            Self::Minutes10 => Timeframe::M5,
            Self::Minutes15 => Timeframe::M15,
            Self::Minutes30 => Timeframe::M30,
            Self::Hour => Timeframe::H1,
            Self::Hours2 => Timeframe::H2,
            Self::Hours4 => Timeframe::H4,
        }
    }
}

impl std::fmt::Display for BlockSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Minutes5 => write!(f, "5 minutes"),
            Self::Minutes10 => write!(f, "10 minutes"),
            Self::Minutes15 => write!(f, "15 minutes"),
            Self::Minutes30 => write!(f, "30 minutes"),
            Self::Hour => write!(f, "1 hour"),
            Self::Hours2 => write!(f, "2 hours"),
            Self::Hours4 => write!(f, "4 hours"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum DisplayStyle {
    #[default]
    Auto,
    Letters,
    Blocks,
}

impl DisplayStyle {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Letters, Self::Blocks];
}

impl std::fmt::Display for DisplayStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "Auto"),
            Self::Letters => write!(f, "Letters"),
            Self::Blocks => write!(f, "Blocks"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub profile_period: ProfilePeriod,
    pub block_size: BlockSize,
    pub ticks_per_row: u32,
    /// UTC minute-of-day that anchors every profile period.
    pub session_start_minutes_utc: u16,
    pub value_area_percent: u8,
    pub initial_balance_blocks: u8,
    pub profiles_to_load: u8,
    pub display_style: DisplayStyle,
    pub show_poc: bool,
    pub show_value_area: bool,
    pub show_initial_balance: bool,
    pub show_single_prints: bool,
}

impl Config {
    pub const TICKS_PER_ROW_PRESETS: [u32; 18] = [
        1, 2, 4, 5, 10, 20, 25, 50, 60, 100, 200, 250, 500, 600, 1_000, 2_000, 5_000, 10_000,
    ];
    pub const VALUE_AREA_PRESETS: [u8; 7] = [50, 60, 68, 70, 75, 80, 90];
    pub const INITIAL_BALANCE_PRESETS: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    /// Keep enough bar-seeded history for meaningful profile comparison.
    pub const MIN_HISTORY_PROFILES: u8 = 150;
    pub const MAX_HISTORY_PROFILES: u8 = 240;
    pub const HISTORY_PROFILE_PRESETS: [u8; 4] = [150, 180, 210, 240];

    pub fn normalized(self) -> Self {
        let block_size = if self.block_size.minutes() > self.profile_period.minutes() {
            match self.profile_period {
                ProfilePeriod::Hour => BlockSize::Hour,
                ProfilePeriod::Hours2 => BlockSize::Hours2,
                ProfilePeriod::Hours4
                | ProfilePeriod::Hours8
                | ProfilePeriod::Hours12
                | ProfilePeriod::Day
                | ProfilePeriod::Week => BlockSize::Hours4,
            }
        } else {
            self.block_size
        };
        let max_blocks = (self.profile_period.minutes() / block_size.minutes()).max(1);
        Self {
            block_size,
            ticks_per_row: self.ticks_per_row.clamp(1, 10_000),
            session_start_minutes_utc: self.session_start_minutes_utc.min(1_439),
            value_area_percent: self.value_area_percent.clamp(1, 100),
            initial_balance_blocks: u8::try_from(
                u64::from(self.initial_balance_blocks.max(1)).min(max_blocks),
            )
            .unwrap_or(u8::MAX),
            profiles_to_load: self
                .profiles_to_load
                .clamp(Self::MIN_HISTORY_PROFILES, Self::MAX_HISTORY_PROFILES),
            ..self
        }
    }

    /// Candle timeframe used to seed letter high–low ranges from exchange klines.
    pub fn letter_timeframe(self) -> Timeframe {
        self.normalized().block_size.letter_timeframe()
    }

    pub fn row_step(self, tick_size: PriceStep) -> PriceStep {
        PriceStep {
            units: tick_size
                .units
                .saturating_mul(i64::from(self.normalized().ticks_per_row)),
        }
    }

    pub fn profile_start(self, time: UnixMs) -> UnixMs {
        let cfg = self.normalized();
        let period = i128::from(cfg.profile_period.millis());
        let week_anchor = if cfg.profile_period == ProfilePeriod::Week {
            i128::from(4 * 24 * 60 * 60 * 1_000_u64)
        } else {
            0
        };
        let offset = week_anchor + i128::from(cfg.session_start_minutes_utc) * 60_000;
        let timestamp = i128::from(time.as_u64());
        let aligned = (timestamp - offset).div_euclid(period) * period + offset;
        UnixMs::new(u64::try_from(aligned.max(0)).unwrap_or(0))
    }

    pub fn block_index(self, profile_start: UnixMs, time: UnixMs) -> u16 {
        let elapsed = time.saturating_diff(profile_start);
        u16::try_from(elapsed / self.normalized().block_size.millis()).unwrap_or(u16::MAX)
    }

    pub fn history_range_ms(self) -> u64 {
        self.normalized()
            .profile_period
            .millis()
            .saturating_mul(u64::from(self.normalized().profiles_to_load))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            profile_period: ProfilePeriod::Day,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 10,
            session_start_minutes_utc: 0,
            value_area_percent: 70,
            initial_balance_blocks: 2,
            // Multi-day history is bar-seeded (cheap), not raw-trade backfill.
            profiles_to_load: Self::MIN_HISTORY_PROFILES,
            display_style: DisplayStyle::Auto,
            show_poc: true,
            show_value_area: true,
            show_initial_balance: true,
            show_single_prints: true,
        }
    }
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TPO {} · {}T",
            match self.block_size {
                BlockSize::Minutes5 => "5m",
                BlockSize::Minutes10 => "10m",
                BlockSize::Minutes15 => "15m",
                BlockSize::Minutes30 => "30m",
                BlockSize::Hour => "1h",
                BlockSize::Hours2 => "2h",
                BlockSize::Hours4 => "4h",
            },
            self.ticks_per_row
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStart(pub u16);

impl SessionStart {
    pub fn half_hour_presets() -> Vec<Self> {
        (0..48).map(|slot| Self(slot * 30)).collect()
    }
}

impl std::fmt::Display for SessionStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let minutes = self.0.min(1_439);
        write!(f, "{:02}:{:02} UTC", minutes / 60, minutes % 60)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub blocks: Vec<u16>,
}

impl Row {
    fn insert_block(&mut self, block: u16) {
        match self.blocks.binary_search(&block) {
            Ok(_) => {}
            Err(index) => self.blocks.insert(index, block),
        }
    }

    pub fn count(&self) -> usize {
        self.blocks.len()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BracketRange {
    pub low: Price,
    pub high: Price,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub start: UnixMs,
    pub end: UnixMs,
    pub rows: BTreeMap<Price, Row>,
    pub brackets: BTreeMap<u16, BracketRange>,
    pub poc: Price,
    pub value_area_high: Price,
    pub value_area_low: Price,
    pub initial_balance_high: Price,
    pub initial_balance_low: Price,
    pub total_tpos: usize,
    pub first_time: UnixMs,
    pub last_time: UnixMs,
    pub open: Price,
    pub close: Price,
}

impl Profile {
    pub fn new(config: Config, row_step: PriceStep, trade: &Trade) -> Self {
        let start = config.profile_start(trade.time);
        let mut profile = Self {
            start,
            end: start.saturating_add(config.normalized().profile_period.millis()),
            rows: BTreeMap::new(),
            brackets: BTreeMap::new(),
            poc: trade.price,
            value_area_high: trade.price,
            value_area_low: trade.price,
            initial_balance_high: trade.price,
            initial_balance_low: trade.price,
            total_tpos: 0,
            first_time: trade.time,
            last_time: trade.time,
            open: trade.price,
            close: trade.price,
        };
        profile.update(config, row_step, trade);
        profile
    }

    pub fn update(&mut self, config: Config, row_step: PriceStep, trade: &Trade) {
        self.apply_trade(config, row_step, trade);
        self.recalculate(config, row_step);
    }

    /// Apply a trade without recomputing POC / value area / IB.
    pub fn apply_trade(&mut self, config: Config, row_step: PriceStep, trade: &Trade) {
        self.apply_price_print(
            config,
            row_step,
            trade.time,
            trade.price,
            trade.price,
            trade.price,
        );
    }

    /// Sierra/Quantower-style letter construction from a time bar.
    ///
    /// For the letter/block that contains `kline.time`, mark every TPO row from
    /// the bar low through the bar high (continuous auction range for that
    /// sub-period). Volume is intentionally unused — TPO is time-at-price.
    pub fn apply_kline(&mut self, config: Config, row_step: PriceStep, kline: &Kline) {
        self.apply_price_print(
            config, row_step, kline.time, kline.open, kline.low, kline.high,
        );
        // Close tracks the last bar's close (developing session last trade proxy).
        if kline.time >= self.last_time {
            self.close = kline.close;
            self.last_time = kline.time;
        }
    }

    /// Expand the letter bracket covering `time` to include `[range_low, range_high]`.
    fn apply_price_print(
        &mut self,
        config: Config,
        row_step: PriceStep,
        time: UnixMs,
        open_price: Price,
        range_low: Price,
        range_high: Price,
    ) {
        if row_step.units <= 0 {
            return;
        }

        let virgin = self.brackets.is_empty();
        if virgin || time < self.first_time {
            self.first_time = time;
            self.open = open_price;
        }
        if virgin || time >= self.last_time {
            self.last_time = time;
            // Trade path passes a single print as both high and low.
            // Klines overwrite `close` with the bar close after this call.
            self.close = if range_low == range_high {
                range_low
            } else {
                open_price
            };
        }

        let low = range_low.min(range_high);
        let high = range_low.max(range_high);
        let block = config.block_index(self.start, time);

        let mut expansions = Vec::new();
        match self.brackets.entry(block) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(BracketRange { low, high });
                expansions.push((price_to_row(low, row_step), price_to_row(high, row_step)));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let range = entry.get_mut();
                let prev_low = range.low;
                let prev_high = range.high;

                if low < range.low {
                    range.low = low;
                    expansions.push((
                        price_to_row(low, row_step),
                        price_to_row(prev_low, row_step),
                    ));
                }
                if high > range.high {
                    range.high = high;
                    expansions.push((
                        price_to_row(prev_high, row_step),
                        price_to_row(high, row_step),
                    ));
                }
            }
        }
        for (a, b) in expansions {
            self.fill_rows(block, a, b, row_step);
        }
    }

    pub fn empty_at(config: Config, start: UnixMs, open: Price) -> Self {
        let cfg = config.normalized();
        Self {
            start,
            end: start.saturating_add(cfg.profile_period.millis()),
            rows: BTreeMap::new(),
            brackets: BTreeMap::new(),
            poc: open,
            value_area_high: open,
            value_area_low: open,
            initial_balance_high: open,
            initial_balance_low: open,
            total_tpos: 0,
            first_time: start,
            last_time: start,
            open,
            close: open,
        }
    }

    /// Hard cap on price rows per profile. Prevents pathological tick sizes or
    /// bad OHLC from allocating multi-million row maps (OOM / silent abort).
    const MAX_ROWS_PER_PROFILE: usize = 20_000;

    fn fill_rows(&mut self, block: u16, low: Price, high: Price, row_step: PriceStep) {
        if row_step.units <= 0 {
            return;
        }
        let mut units = low.units;
        let mut filled = 0usize;
        while units <= high.units {
            if self.rows.len() >= Self::MAX_ROWS_PER_PROFILE || filled >= Self::MAX_ROWS_PER_PROFILE
            {
                break;
            }
            self.rows
                .entry(Price::from_units(units))
                .or_default()
                .insert_block(block);
            filled = filled.saturating_add(1);
            let next = units.saturating_add(row_step.units);
            if next <= units {
                break;
            }
            units = next;
        }
    }

    pub fn recalculate(&mut self, config: Config, row_step: PriceStep) {
        let Some((&low, _)) = self.rows.first_key_value() else {
            return;
        };
        let Some((&high, _)) = self.rows.last_key_value() else {
            return;
        };
        let midpoint_units = (i128::from(low.units) + i128::from(high.units)) / 2;

        self.total_tpos = self.rows.values().map(Row::count).sum();
        // POC: longest TPO row; ties break toward the profile midpoint, then the lower row.
        self.poc = self
            .rows
            .iter()
            .max_by(|(price_a, row_a), (price_b, row_b)| {
                row_a.count().cmp(&row_b.count()).then_with(|| {
                    let dist_a = (i128::from(price_a.units) - midpoint_units).abs();
                    let dist_b = (i128::from(price_b.units) - midpoint_units).abs();
                    dist_b.cmp(&dist_a).then_with(|| price_b.cmp(price_a))
                })
            })
            .map_or(low, |(price, _)| *price);

        let prices = self.rows.keys().copied().collect::<Vec<_>>();
        let Ok(poc_index) = prices.binary_search(&self.poc) else {
            // Should be impossible if POC was taken from `rows`, but never panic
            // on the UI thread — leave VAH/VAL at POC and return.
            self.value_area_high = self.poc;
            self.value_area_low = self.poc;
            return;
        };
        let target = self
            .total_tpos
            .saturating_mul(usize::from(config.normalized().value_area_percent))
            .div_ceil(100);
        let mut included = self.rows[&self.poc].count();
        let mut low_index = poc_index;
        let mut high_index = poc_index;

        // Standard Market Profile value area: expand from the POC one row at a
        // time, always taking the side with more TPOs (both when tied).
        while included < target && (low_index > 0 || high_index + 1 < prices.len()) {
            let below = low_index.checked_sub(1);
            let above = (high_index + 1 < prices.len()).then_some(high_index + 1);
            match (below, above) {
                (Some(below), Some(above)) => {
                    let below_count = self.rows[&prices[below]].count();
                    let above_count = self.rows[&prices[above]].count();
                    match above_count.cmp(&below_count) {
                        std::cmp::Ordering::Greater => {
                            high_index = above;
                            included += above_count;
                        }
                        std::cmp::Ordering::Less => {
                            low_index = below;
                            included += below_count;
                        }
                        std::cmp::Ordering::Equal => {
                            low_index = below;
                            high_index = above;
                            included += below_count + above_count;
                        }
                    }
                }
                (Some(below), None) => {
                    low_index = below;
                    included += self.rows[&prices[below]].count();
                }
                (None, Some(above)) => {
                    high_index = above;
                    included += self.rows[&prices[above]].count();
                }
                (None, None) => break,
            }
        }

        self.value_area_low = prices[low_index];
        self.value_area_high = prices[high_index];

        let ib_blocks = u16::from(config.normalized().initial_balance_blocks);
        let mut ib_low = None;
        let mut ib_high = None;
        for (_, range) in self.brackets.range(..ib_blocks) {
            ib_low = Some(ib_low.map_or(range.low, |value: Price| value.min(range.low)));
            ib_high = Some(ib_high.map_or(range.high, |value: Price| value.max(range.high)));
        }
        // IB edges snap to the same row grid as letter cells.
        self.initial_balance_low = price_to_row(ib_low.unwrap_or(low), row_step);
        self.initial_balance_high = price_to_row(ib_high.unwrap_or(high), row_step);
    }

    /// True when `price` is an interior single-print row.
    ///
    /// Single prints are one-TPO rows surrounded by profile structure. A
    /// continuous one-TPO run connected to the profile high or low is excess
    /// (a buying/selling tail), not an interior single print. A developing
    /// one-letter profile therefore does not paint every row as a single.
    pub fn is_single_print(&self, price: Price) -> bool {
        if self.rows.get(&price).is_none_or(|row| row.count() != 1) {
            return false;
        }

        let prices: Vec<Price> = self.rows.keys().copied().collect();
        let Ok(index) = prices.binary_search(&price) else {
            return false;
        };
        let is_single = |idx: usize| self.rows[&prices[idx]].count() == 1;

        let connected_to_high = (index..prices.len()).all(is_single);
        let connected_to_low = (0..=index).all(is_single);
        !connected_to_high && !connected_to_low
    }
}

/// Map a raw price onto its nearest TPO row increment.
///
/// Sierra Chart rounds underlying prices to the nearest multiple of the
/// configured letter/block price increment. Half-step prices round upward.
pub fn price_to_row(price: Price, row_step: PriceStep) -> Price {
    if row_step.units <= 0 {
        return price;
    }

    let rounded_units = price
        .units
        .saturating_add(row_step.units / 2)
        .div_euclid(row_step.units)
        .saturating_mul(row_step.units);
    Price::from_units(rounded_units)
}

pub fn block_letter(index: u16) -> char {
    let code = index % 52;
    if code < 26 {
        char::from(b'A' + u8::try_from(code).unwrap_or(0))
    } else {
        char::from(b'a' + u8::try_from(code - 26).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dollar(value: f64) -> Price {
        Price::from_f64(value)
    }

    fn dollar_step() -> PriceStep {
        PriceStep {
            units: dollar(1.0).units,
        }
    }

    fn row(blocks: &[u16]) -> Row {
        Row {
            blocks: blocks.to_vec(),
        }
    }

    #[test]
    fn profile_periods_align_to_session_start_and_monday() {
        let daily = Config {
            session_start_minutes_utc: 9 * 60 + 30,
            ..Config::default()
        };
        let time = UnixMs::new((24 + 8) * 60 * 60 * 1_000);
        assert_eq!(
            daily.profile_start(time),
            UnixMs::new((9 * 60 + 30) * 60 * 1_000)
        );

        let weekly = Config {
            profile_period: ProfilePeriod::Week,
            session_start_minutes_utc: 0,
            ..Config::default()
        };
        assert_eq!(
            weekly.profile_start(UnixMs::new(8 * 24 * 60 * 60 * 1_000)),
            UnixMs::new(4 * 24 * 60 * 60 * 1_000)
        );
    }

    #[test]
    fn poc_ties_use_profile_midpoint_then_lower_row() {
        let mut profile = Profile {
            start: UnixMs::new(0),
            end: UnixMs::new(86_400_000),
            rows: BTreeMap::from([
                (dollar(98.0), row(&[0])),
                (dollar(99.0), row(&[0, 1])),
                (dollar(100.0), row(&[0, 1, 2])),
                (dollar(101.0), row(&[0, 1, 2])),
                (dollar(102.0), row(&[0])),
            ]),
            brackets: BTreeMap::new(),
            poc: dollar(0.0),
            value_area_high: dollar(0.0),
            value_area_low: dollar(0.0),
            initial_balance_high: dollar(0.0),
            initial_balance_low: dollar(0.0),
            total_tpos: 0,
            first_time: UnixMs::new(0),
            last_time: UnixMs::new(0),
            open: dollar(100.0),
            close: dollar(100.0),
        };

        profile.recalculate(Config::default(), dollar_step());
        assert_eq!(profile.poc, dollar(100.0));
    }

    #[test]
    fn value_area_expands_from_poc_and_includes_equal_sides() {
        let mut profile = Profile {
            start: UnixMs::new(0),
            end: UnixMs::new(86_400_000),
            rows: BTreeMap::from([
                (dollar(98.0), row(&[0])),
                (dollar(99.0), row(&[0, 1])),
                (dollar(100.0), row(&[0, 1, 2])),
                (dollar(101.0), row(&[0, 1])),
                (dollar(102.0), row(&[0])),
            ]),
            brackets: BTreeMap::new(),
            poc: dollar(0.0),
            value_area_high: dollar(0.0),
            value_area_low: dollar(0.0),
            initial_balance_high: dollar(0.0),
            initial_balance_low: dollar(0.0),
            total_tpos: 0,
            first_time: UnixMs::new(0),
            last_time: UnixMs::new(0),
            open: dollar(100.0),
            close: dollar(100.0),
        };

        profile.recalculate(Config::default(), dollar_step());
        assert_eq!(profile.total_tpos, 9);
        assert_eq!(profile.poc, dollar(100.0));
        assert_eq!(profile.value_area_low, dollar(99.0));
        assert_eq!(profile.value_area_high, dollar(101.0));
    }

    #[test]
    fn config_normalizes_invalid_combinations_and_roundtrips() {
        let config = Config {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Hours4,
            ticks_per_row: 0,
            session_start_minutes_utc: 2_000,
            value_area_percent: 0,
            initial_balance_blocks: 8,
            profiles_to_load: 0,
            display_style: DisplayStyle::Letters,
            show_poc: false,
            show_value_area: false,
            show_initial_balance: false,
            show_single_prints: false,
        }
        .normalized();

        assert_eq!(config.block_size, BlockSize::Hour);
        assert_eq!(config.ticks_per_row, 1);
        assert_eq!(config.session_start_minutes_utc, 1_439);
        assert_eq!(config.value_area_percent, 1);
        assert_eq!(config.initial_balance_blocks, 1);
        assert_eq!(config.profiles_to_load, Config::MIN_HISTORY_PROFILES);

        let json = serde_json::to_string(&config).expect("serialize TPO settings");
        assert_eq!(
            serde_json::from_str::<Config>(&json).expect("deserialize TPO settings"),
            config
        );
    }

    #[test]
    fn legacy_history_depth_is_upgraded_to_at_least_150_profiles() {
        let legacy = Config {
            profiles_to_load: 60,
            ..Config::default()
        };

        assert_eq!(
            legacy.normalized().profiles_to_load,
            Config::MIN_HISTORY_PROFILES
        );
        assert_eq!(
            legacy.history_range_ms(),
            ProfilePeriod::Day.millis() * u64::from(Config::MIN_HISTORY_PROFILES)
        );
    }

    #[test]
    fn chart_kind_roundtrip_preserves_every_tpo_setting() {
        let config = Config {
            profile_period: ProfilePeriod::Hours8,
            block_size: BlockSize::Minutes15,
            ticks_per_row: 25,
            session_start_minutes_utc: 8 * 60 + 30,
            value_area_percent: 75,
            initial_balance_blocks: 4,
            profiles_to_load: 180,
            display_style: DisplayStyle::Blocks,
            show_poc: false,
            show_value_area: true,
            show_initial_balance: false,
            show_single_prints: true,
        };
        let kind = super::super::kline::KlineChartKind::Tpo { config };

        let json = serde_json::to_string(&kind).expect("serialize TPO chart kind");
        assert_eq!(
            serde_json::from_str::<super::super::kline::KlineChartKind>(&json)
                .expect("deserialize TPO chart kind"),
            kind
        );
    }

    #[test]
    fn renko_construction_and_visual_settings_roundtrip() {
        use super::super::kline::{Config as VisualConfig, KlineChartKind, RenkoConfig};

        let kind = KlineChartKind::Renko {
            config: RenkoConfig {
                brick_size: 350,
                reversal: 3,
                normalization_ms: 500,
            },
        };
        let visual = VisualConfig {
            data_labels_always_visible: true,
            show_footprint_summary: false,
            show_renko_wicks: false,
            daily_delta_ticks: 10,
            daily_delta_days: 5,
            previous_value_area_ticks: 10,
            liquidity_heatmap_order_size_filter: 125_000.0,
        };

        let kind_json = serde_json::to_string(&kind).expect("serialize Renko settings");
        let visual_json = serde_json::to_string(&visual).expect("serialize Renko visual settings");
        assert_eq!(
            serde_json::from_str::<KlineChartKind>(&kind_json).expect("deserialize Renko settings"),
            kind
        );
        assert_eq!(
            serde_json::from_str::<VisualConfig>(&visual_json)
                .expect("deserialize Renko visual settings"),
            visual
        );
    }

    #[test]
    fn letter_sequence_matches_standard_upper_then_lower_cycle() {
        assert_eq!(block_letter(0), 'A');
        assert_eq!(block_letter(25), 'Z');
        assert_eq!(block_letter(26), 'a');
        assert_eq!(block_letter(51), 'z');
        assert_eq!(block_letter(52), 'A');
    }

    #[test]
    fn each_price_maps_to_the_nearest_row() {
        let step = dollar_step();
        assert_eq!(price_to_row(dollar(100.49), step), dollar(100.0));
        assert_eq!(price_to_row(dollar(100.5), step), dollar(101.0));
        assert_eq!(price_to_row(dollar(100.0), step), dollar(100.0));
        assert_eq!(price_to_row(dollar(100.99), step), dollar(101.0));

        let config = Config {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            ..Config::default()
        };
        let trade = Trade {
            time: UnixMs::new(0),
            is_sell: false,
            price: dollar(100.5),
            qty: exchange::unit::Qty::from_f64(1.0),
        };
        let profile = Profile::new(config, step, &trade);
        assert_eq!(profile.rows.len(), 1);
        assert!(!profile.rows.contains_key(&dollar(100.0)));
        assert!(profile.rows.contains_key(&dollar(101.0)));
    }

    #[test]
    fn bracket_range_fills_inclusive_nearest_rows_only() {
        let config = Config {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            ..Config::default()
        };
        let step = dollar_step();
        let mut profile = Profile::new(
            config,
            step,
            &Trade {
                time: UnixMs::new(0),
                is_sell: false,
                price: dollar(100.1),
                qty: exchange::unit::Qty::from_f64(1.0),
            },
        );
        profile.update(
            config,
            step,
            &Trade {
                time: UnixMs::new(1_000),
                is_sell: false,
                price: dollar(102.9),
                qty: exchange::unit::Qty::from_f64(1.0),
            },
        );

        // nearest(100.1)=100, nearest(102.9)=103 -> rows 100 through 103.
        assert_eq!(
            profile.rows.keys().copied().collect::<Vec<_>>(),
            vec![dollar(100.0), dollar(101.0), dollar(102.0), dollar(103.0)]
        );
    }

    #[test]
    fn single_prints_are_interior_one_tpo_rows_not_excess_tails() {
        let mut profile = Profile {
            start: UnixMs::new(0),
            end: UnixMs::new(86_400_000),
            rows: BTreeMap::from([
                (dollar(98.0), row(&[0])),        // bottom excess
                (dollar(99.0), row(&[0])),        // bottom excess
                (dollar(100.0), row(&[0, 1, 2])), // body
                (dollar(101.0), row(&[2])),       // interior single
                (dollar(102.0), row(&[0, 1])),    // body
                (dollar(103.0), row(&[2])),       // top excess
                (dollar(104.0), row(&[2])),       // top excess
            ]),
            brackets: BTreeMap::new(),
            poc: dollar(0.0),
            value_area_high: dollar(0.0),
            value_area_low: dollar(0.0),
            initial_balance_high: dollar(0.0),
            initial_balance_low: dollar(0.0),
            total_tpos: 0,
            first_time: UnixMs::new(0),
            last_time: UnixMs::new(0),
            open: dollar(100.0),
            close: dollar(100.0),
        };
        profile.recalculate(Config::default(), dollar_step());

        assert!(!profile.is_single_print(dollar(98.0)));
        assert!(!profile.is_single_print(dollar(99.0)));
        assert!(!profile.is_single_print(dollar(100.0)));
        assert!(profile.is_single_print(dollar(101.0)));
        assert!(!profile.is_single_print(dollar(102.0)));
        assert!(!profile.is_single_print(dollar(103.0)));
        assert!(!profile.is_single_print(dollar(104.0)));
    }

    #[test]
    fn developing_one_letter_profile_is_not_all_singles() {
        let config = Config {
            profile_period: ProfilePeriod::Hour,
            block_size: BlockSize::Minutes30,
            ticks_per_row: 1,
            ..Config::default()
        };
        let step = dollar_step();
        let mut profile = Profile::new(
            config,
            step,
            &Trade {
                time: UnixMs::new(0),
                is_sell: false,
                price: dollar(100.0),
                qty: exchange::unit::Qty::from_f64(1.0),
            },
        );
        profile.update(
            config,
            step,
            &Trade {
                time: UnixMs::new(1_000),
                is_sell: false,
                price: dollar(103.0),
                qty: exchange::unit::Qty::from_f64(1.0),
            },
        );

        // One letter only -> every row is connected to an extreme, so nothing
        // is flagged as an interior single print.
        assert!(profile.rows.values().all(|row| row.count() == 1));
        assert!(
            profile
                .rows
                .keys()
                .all(|price| !profile.is_single_print(*price))
        );
    }
}
