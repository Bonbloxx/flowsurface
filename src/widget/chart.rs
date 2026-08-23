pub mod comparison;
pub mod heatmap;

use chrono::{TimeZone, Utc};
use exchange::TickerInfo;
use iced::mouse;

const WHEEL_SCROLL_PIXELS_PER_NOTCH: f32 = 120.0;
const WHEEL_ZOOM_BASE_PER_NOTCH: f32 = 1.16;
const MAX_ABS_WHEEL_NOTCHES_PER_EVENT: f32 = 4.0;

pub(crate) fn wheel_zoom_factor(delta: mouse::ScrollDelta) -> f32 {
    let notches = match delta {
        mouse::ScrollDelta::Lines { y, .. } => y,
        mouse::ScrollDelta::Pixels { y, .. } => y / WHEEL_SCROLL_PIXELS_PER_NOTCH,
    }
    .clamp(
        -MAX_ABS_WHEEL_NOTCHES_PER_EVENT,
        MAX_ABS_WHEEL_NOTCHES_PER_EVENT,
    );

    WHEEL_ZOOM_BASE_PER_NOTCH.powf(notches)
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Zoom(pub usize);

impl Zoom {
    /// "show all data"
    pub fn all() -> Self {
        Self(0)
    }
    pub fn points(n: usize) -> Self {
        Self(n)
    }
    pub fn is_all(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone)]
pub struct Series {
    pub ticker_info: TickerInfo,
    pub name: Option<String>,
    pub points: Vec<(u64, f32)>,
    pub color: iced::Color,
}

impl Series {
    pub fn new(ticker_info: TickerInfo, color: iced::Color, name: Option<String>) -> Self {
        Self {
            ticker_info,
            name,
            points: Vec::new(),
            color,
        }
    }
}

pub trait SeriesLike {
    fn name(&self) -> String;
    fn points(&self) -> &[(u64, f32)];
    fn color(&self) -> iced::Color;
    fn ticker_info(&self) -> &TickerInfo;
}

impl SeriesLike for Series {
    fn name(&self) -> String {
        if let Some(name) = &self.name {
            name.clone()
        } else {
            self.ticker_info.ticker.to_string()
        }
    }

    fn points(&self) -> &[(u64, f32)] {
        &self.points
    }

    fn color(&self) -> iced::Color {
        self.color
    }

    fn ticker_info(&self) -> &TickerInfo {
        &self.ticker_info
    }
}

/// Compute a "nice" step close to range/target using 1/2/5*10^k
fn nice_step_multiplier_125(v: f32) -> f32 {
    if v <= 1.0 {
        1.0
    } else if v <= 2.0 {
        2.0
    } else if v <= 5.0 {
        5.0
    } else {
        10.0
    }
}

fn nice_step(rough: f32) -> f32 {
    if !rough.is_finite() || rough <= 0.0 {
        return 1.0;
    }

    let base = 10.0f32.powf(rough.log10().floor());
    let fraction = rough / base;

    nice_step_multiplier_125(fraction) * base
}

fn ticks(min: f32, max: f32, target: usize) -> (Vec<f32>, f32) {
    let span = (max - min).abs().max(1e-6);

    let step = {
        let target = target.max(2) as f32;
        let raw = (span / target).max(f32::EPSILON);
        nice_step(raw)
    };

    let start = (min / step).floor() * step;
    let end = (max / step).ceil() * step;

    let mut v = Vec::new();
    let mut t = start;
    for _ in 0..100 {
        if t > end + step * 0.5 {
            break;
        }
        v.push(t);
        t += step;
    }
    (v, step)
}

fn format_pct(val: f32, step: f32, show_decimals: bool) -> String {
    if show_decimals {
        if step >= 1.0 {
            format!("{:+.1}%", val)
        } else if step >= 0.1 {
            format!("{:+.2}%", val)
        } else {
            format!("{:+.3}%", val)
        }
    } else if step >= 1.0 {
        format!("{:+.0}%", val)
    } else if step >= 0.1 {
        format!("{:+.1}%", val)
    } else {
        format!("{:+.2}%", val)
    }
}

fn time_tick_candidates() -> &'static [u64] {
    const S: u64 = 1_000;
    const M: u64 = 60 * S;
    const H: u64 = 60 * M;
    const D: u64 = 24 * H;
    &[
        S,
        2 * S,
        5 * S,
        10 * S,
        15 * S,
        30 * S, //
        M,
        2 * M,
        5 * M,
        10 * M,
        15 * M,
        30 * M, //
        H,
        2 * H,
        4 * H,
        6 * H,
        12 * H, //
        D,
        2 * D,
        7 * D,
        14 * D, //
        30 * D,
        90 * D,
        180 * D,
        365 * D,
    ]
}

fn format_time_label(ts_ms: u64, step_ms: u64) -> String {
    let Some(dt) = Utc.timestamp_millis_opt(ts_ms as i64).single() else {
        return String::new();
    };

    const S: u64 = 1_000;
    const M: u64 = 60 * S;
    const H: u64 = 60 * M;
    const D: u64 = 24 * H;

    if step_ms < M {
        dt.format("%H:%M:%S").to_string()
    } else if step_ms < D {
        dt.format("%H:%M").to_string()
    } else if step_ms < 7 * D {
        dt.format("%b %d").to_string()
    } else if step_ms < 365 * D {
        dt.format("%Y-%m").to_string()
    } else {
        dt.format("%Y").to_string()
    }
}

fn time_ticks(min_x: u64, max_x: u64, px_per_ms: f32, min_px: f32) -> (Vec<u64>, u64) {
    let span = max_x.saturating_sub(min_x).max(1);
    let mut step = *time_tick_candidates().first().unwrap_or(&1_000);
    for &candidate in time_tick_candidates() {
        let px = candidate as f32 * px_per_ms;
        if px >= min_px {
            step = candidate;
            break;
        } else {
            step = candidate;
        }
    }
    // Align first tick to the step boundary >= min_x
    let first = if min_x.is_multiple_of(step) {
        min_x
    } else {
        (min_x / step + 1) * step
    };
    let mut out = Vec::new();
    let mut t = first;
    for _ in 0..=2000 {
        if t > max_x {
            break;
        }
        out.push(t);
        t = t.saturating_add(step);
        if (t - first) > span + step {
            break;
        }
    }
    (out, step)
}

#[cfg(test)]
mod wheel_zoom_tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1.0e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn one_notch_uses_the_stronger_scale_step_in_both_directions() {
        let zoom_in = wheel_zoom_factor(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 });
        let zoom_out = wheel_zoom_factor(mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 });

        assert_close(zoom_in, 1.16);
        assert_close(zoom_in * zoom_out, 1.0);
    }

    #[test]
    fn pixel_and_line_wheel_events_have_the_same_notch_scale() {
        let line = wheel_zoom_factor(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 });
        let pixels = wheel_zoom_factor(mouse::ScrollDelta::Pixels { x: 0.0, y: 120.0 });

        assert_close(line, pixels);
    }

    #[test]
    fn unusually_large_wheel_events_are_bounded() {
        let bounded = wheel_zoom_factor(mouse::ScrollDelta::Lines { x: 0.0, y: 4.0 });
        let noisy = wheel_zoom_factor(mouse::ScrollDelta::Lines { x: 0.0, y: 100.0 });

        assert_close(noisy, bounded);
    }
}

pub mod domain {
    pub fn align_floor(ts: u64, dt: u64) -> u64 {
        if dt == 0 {
            return ts;
        }
        (ts / dt) * dt
    }

    pub fn align_ceil(ts: u64, dt: u64) -> u64 {
        if dt == 0 {
            return ts;
        }
        let f = (ts / dt) * dt;
        if f == ts { ts } else { f.saturating_add(dt) }
    }

    pub fn interpolate_y_at(points: &[(u64, f32)], x: u64) -> Option<f32> {
        if points.is_empty() {
            return None;
        }
        let idx_right = points.iter().position(|(px, _)| *px >= x)?;
        Some(match idx_right {
            0 => points[0].1,
            i => {
                let (x0, y0) = points[i - 1];
                let (x1, y1) = points[i];
                let dx = x1.saturating_sub(x0) as f32;
                if dx > 0.0 {
                    let t = (x.saturating_sub(x0)) as f32 / dx;
                    y0 + (y1 - y0) * t.clamp(0.0, 1.0)
                } else {
                    y0
                }
            }
        })
    }

    pub fn window(
        series: &[&[(u64, f32)]],
        zoom: super::Zoom,
        pan_points: f32,
        dt: u64,
    ) -> Option<(u64, u64)> {
        if series.is_empty() {
            return None;
        }

        let mut any = false;
        let mut data_min_x = u64::MAX;
        let mut data_max_x = u64::MIN;
        for pts in series {
            for (x, _) in *pts {
                any = true;
                if *x < data_min_x {
                    data_min_x = *x;
                }
                if *x > data_max_x {
                    data_max_x = *x;
                }
            }
        }
        if !any {
            return None;
        }
        if data_max_x == data_min_x {
            data_max_x = data_max_x.saturating_add(1);
        }

        let add_signed = |v: u64, d: i64| -> u64 {
            if d >= 0 {
                v.saturating_add(d as u64)
            } else {
                v.saturating_sub((-d) as u64)
            }
        };

        let span = if zoom.is_all() {
            data_max_x.saturating_sub(data_min_x).max(1)
        } else {
            let n = zoom.0;
            let mut s = ((n.saturating_sub(1)) as u64).saturating_mul(dt);
            if s == 0 {
                s = 1;
            }
            s
        };

        let pad_ms = (pan_points * dt as f32).round() as i64;
        let mut right = add_signed(data_max_x, pad_ms);
        let right_cap = data_max_x.saturating_add(span);
        if right > right_cap {
            right = right_cap;
        }
        let left = right.saturating_sub(span);

        let left = align_floor(left, dt);
        let right = align_ceil(right, dt);

        Some((left, right))
    }

    pub fn pct_domain(series: &[&[(u64, f32)]], min_x: u64, max_x: u64) -> Option<(f32, f32)> {
        let mut min_pct = f32::INFINITY;
        let mut max_pct = f32::NEG_INFINITY;
        let mut any = false;

        for pts in series {
            if pts.is_empty() {
                continue;
            }

            let y0 = interpolate_y_at(pts, min_x).unwrap_or(0.0);
            if y0 == 0.0 {
                continue;
            }

            let mut has_visible = false;
            for (_x, y) in pts.iter().filter(|(x, _)| *x >= min_x && *x <= max_x) {
                has_visible = true;
                let pct = ((*y / y0) - 1.0) * 100.0;
                if pct < min_pct {
                    min_pct = pct;
                }
                if pct > max_pct {
                    max_pct = pct;
                }
            }

            if has_visible {
                any = true;
                if 0.0 < min_pct {
                    min_pct = 0.0;
                }
                if 0.0 > max_pct {
                    max_pct = 0.0;
                }
            }
        }

        if !any {
            return None;
        }

        if (max_pct - min_pct).abs() < f32::EPSILON {
            min_pct -= 1.0;
            max_pct += 1.0;
        }

        let span = (max_pct - min_pct).max(1e-6);
        let pad = span * 0.05;
        Some((min_pct - pad, max_pct + pad))
    }
}
