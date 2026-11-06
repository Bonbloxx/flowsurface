use super::{BasisSeriesExt, KlineIndicatorImpl};
use crate::chart::{
    Caches, Message, ViewState,
    indicator::{
        indicator_row,
        plot::{
            AnySeries, Plot, PlotTooltip, Series, TooltipFn, YScale,
            bar::{BarClass, BarPlot},
        },
    },
};
use crate::style;
use data::chart::{
    BasisSeries, PlotData,
    kline::KlineDataPoint,
    trade_pace::{Config, PacePoint, RETENTION_SECS, TradePace},
};
use exchange::{
    SizeUnit, TickerInfo, Trade, UnixMs, adapter::MarketKind, unit::qty::volume_size_unit,
};
use iced::{
    Element, Length, Point, Theme,
    widget::{canvas, column, container, pick_list, row, space, text},
};
use std::{collections::BTreeMap, ops::RangeInclusive};

pub struct TradePaceIndicator {
    cache: Caches,
    pace: TradePace,
    sources: Vec<(TickerInfo, bool)>,
    tick_data: BasisSeries<PacePoint>,
}

impl TradePaceIndicator {
    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            pace: TradePace::new(Config::default(), UnixMs::now()),
            sources: Vec::new(),
            tick_data: BasisSeries::tick(BTreeMap::new()),
        }
    }
    fn set_connection(&mut self, source: TickerInfo, connected: bool, now: UnixMs) {
        if let Some((_, state)) = self
            .sources
            .iter_mut()
            .find(|(info, _)| info.ticker.same_market(&source.ticker))
        {
            *state = connected;
            self.pace
                .set_connected(self.sources.iter().all(|(_, connected)| *connected), now);
        }
    }
}

impl KlineIndicatorImpl for TradePaceIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
    }
    fn clear_crosshair_caches(&mut self) {
        self.cache.crosshair.clear();
    }

    fn configure_trade_pace(&mut self, sources: &[TickerInfo], config: Config) {
        if !self.sources.iter().map(|(info, _)| info).eq(sources.iter()) {
            self.sources = sources
                .iter()
                .copied()
                .map(|source| (source, false))
                .collect();
            self.pace = TradePace::new(config, UnixMs::now());
            self.tick_data = BasisSeries::tick(BTreeMap::new());
        } else {
            self.pace.configure(config);
        }
        self.clear_all_caches();
    }

    fn trade_pace_connection(&mut self, source: TickerInfo, connected: bool, now: UnixMs) {
        self.set_connection(source, connected, now);
        self.clear_all_caches();
    }

    fn on_source_trades(&mut self, source: TickerInfo, trades: &[Trade], historical: bool) {
        if historical
            || trades.is_empty()
            || !self
                .sources
                .iter()
                .any(|(info, _)| info.ticker.same_market(&source.ticker))
        {
            return;
        }
        let now = UnixMs::now();
        self.set_connection(source, true, now);
        let qty_is_quote = volume_size_unit() == SizeUnit::Quote
            || source.market_type() == MarketKind::InversePerps;
        self.pace.insert(trades, qty_is_quote, now);
        // Deliberately do not clear geometry or rebuild chart buckets per tape batch.
    }

    fn maintain_trade_pace(&mut self, now: UnixMs, source: &PlotData<KlineDataPoint>) -> bool {
        if !self.pace.publish(now) {
            return false;
        }
        if let PlotData::TickBased(ticks) = source {
            let data = self.tick_data.tick_mut().expect("tick projection");
            data.clear();
            let start = ticks
                .datapoints
                .len()
                .saturating_sub(RETENTION_SECS as usize);
            let latest = self.pace.points().last_key_value().map(|(time, _)| *time);
            for index in start..ticks.datapoints.len() {
                let end = ticks
                    .datapoints
                    .get(index + 1)
                    .map(|next| next.kline.time)
                    .or(latest);
                if let Some(end) = end {
                    let time = end.floor_to(exchange::Timeframe::MS1000);
                    // Never carry an old rate across an uncovered part of a Renko/tick chart.
                    if let Some((_, point)) = self.pace.points().range(time..=time).next() {
                        data.insert(index as u64, *point);
                    }
                }
            }
        }
        self.clear_all_caches();
        true
    }

    fn on_basis_change(&mut self, _source: &PlotData<KlineDataPoint>) {
        self.tick_data = BasisSeries::tick(BTreeMap::new());
        self.clear_all_caches();
    }

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        always_labels: bool,
        range: RangeInclusive<u64>,
    ) -> Element<'a, Message> {
        let config = self.pace.config();
        let latest = self.pace.points().last_key_value().map(|(_, value)| *value);
        let connected = !self.sources.is_empty() && self.sources.iter().all(|(_, state)| *state);
        let readout = latest
            .filter(|point| point.complete && connected)
            .map_or_else(
                || {
                    if connected {
                        "Warming up".to_string()
                    } else {
                        "Waiting for trade venues".to_string()
                    }
                },
                |point| {
                    format!(
                        "{:.1} prints/s   Buy {:.1}   Sell {:.1}   {}",
                        point.total(),
                        point.buy,
                        point.sell,
                        point.baseline.map_or_else(
                            || "Baseline warming up".into(),
                            |baseline| {
                                let relative = point
                                    .relative()
                                    .map_or_else(String::new, |ratio| format!(" · {ratio:.2}×"));
                                format!("Baseline {baseline:.1}{relative}")
                            }
                        )
                    )
                },
            );
        let venues = self
            .sources
            .iter()
            .map(|(source, _)| source.exchange().venue().to_string())
            .collect::<Vec<_>>()
            .join(" + ");
        let title = text("Trade Pace")
            .font(style::AZERET_MONO)
            .size(style::text_size::SMALL);
        let stats = text(readout)
            .font(style::AZERET_MONO)
            .size(style::text_size::SMALL);
        let controls = row![
            pick_list(
                [1_u16, 5, 15, 30, 60],
                Some(config.window_secs),
                Message::TradePaceWindowChanged
            )
            .text_size(style::text_size::SMALL),
            text(format!("s · baseline {}s", config.baseline_secs)).size(style::text_size::SMALL)
        ]
        .spacing(8);
        let header = if chart.bounds.width < 900.0 {
            column![
                row![title, space::horizontal(), controls].spacing(14),
                stats
            ]
            .spacing(4)
        } else {
            column![row![title, stats, space::horizontal(), controls].spacing(14)]
        };
        let series = match chart.basis {
            data::chart::Basis::Time(_) => AnySeries::forward_unix_ms(self.pace.points()),
            data::chart::Basis::Tick(_) => self.tick_data.as_plot_series(),
        };
        let plot = PacePlot { config };
        let legend = row![
            text("Buy taker")
                .size(style::text_size::TINY)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(theme.extended_palette().success.base.color),
                }),
            text("Sell taker")
                .size(style::text_size::TINY)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(theme.extended_palette().danger.base.color),
                }),
            text("Dotted baseline")
                .size(style::text_size::TINY)
                .style(|theme: &Theme| iced::widget::text::Style {
                    color: Some(baseline_color(theme)),
                }),
        ]
        .spacing(12);
        let source_label = text(format!("{venues} · live prints"))
            .font(style::AZERET_MONO)
            .size(style::text_size::TINY);
        let footer = if chart.bounds.width < 650.0 {
            column![source_label, legend].spacing(3)
        } else {
            column![row![source_label, space::horizontal(), legend].spacing(12)]
        };
        column![
            container(header).padding([4, 8]),
            indicator_row(chart, &self.cache, always_labels, plot, series, range),
            container(footer).padding([2, 8])
        ]
        .height(Length::Fill)
        .into()
    }
}

struct PacePlot {
    config: Config,
}

impl PacePlot {
    fn bars(
        &self,
        interval_ms: u64,
    ) -> BarPlot<impl Fn(&PacePoint) -> f32, impl Fn(&PacePoint) -> BarClass, PacePoint> {
        BarPlot::new(
            |point: &PacePoint| if point.complete { point.total() } else { 0.0 },
            |point: &PacePoint| BarClass::Stacked { lower: point.buy },
        )
        .time_interval_ms(interval_ms)
        .padding(0.08)
    }
}

impl<S: Series<Y = PacePoint>> Plot<S> for PacePlot {
    fn time_step_ms(&self) -> Option<u64> {
        Some(1_000)
    }
    fn y_extents(&self, series: &S, range: RangeInclusive<u64>) -> Option<(f32, f32)> {
        let mut high = 0.0_f32;
        series.for_each_in(range, |_, point| {
            if point.complete {
                high = high.max(point.total()).max(point.baseline.unwrap_or(0.0));
            }
        });
        Some((0.0, high.max(1.0) * 1.08))
    }
    fn draw<'a>(
        &'a self,
        frame: &'a mut canvas::Frame,
        chart: &'a ViewState,
        theme: &Theme,
        series: &S,
        range: RangeInclusive<u64>,
        scale: &YScale,
    ) {
        let units_per_pixel = match chart.basis {
            data::chart::Basis::Time(tf) => tf.to_milliseconds() as f32 / 1_000.0,
            data::chart::Basis::Tick(_) => 1.0,
        } / (chart.cell_width * chart.scaling).max(0.001);
        let step = units_per_pixel.ceil().max(1.0) as u64;
        if step > 1 {
            let interval = match chart.basis {
                data::chart::Basis::Time(_) => step * 1_000,
                data::chart::Basis::Tick(_) => step,
            };
            let projected = pixel_series(series, range.clone(), interval);
            self.draw_series(frame, chart, theme, &(&projected), range, scale, interval);
        } else {
            self.draw_series(frame, chart, theme, series, range, scale, 1_000);
        }
    }
    fn tooltip_fn(&self) -> Option<&TooltipFn<PacePoint>> {
        None
    }
    fn tooltip(
        &self,
        point: &PacePoint,
        _next: Option<&PacePoint>,
        _theme: &Theme,
    ) -> Option<PlotTooltip> {
        let time = chrono::DateTime::from_timestamp_millis(point.time.as_u64() as i64)
            .map(|time| time.format("%H:%M:%S UTC").to_string())
            .unwrap_or_default();
        Some(PlotTooltip::new(format!(
            "{time}\nWindow {}s\nTotal {:.1} prints/s\nBuy {:.1} · Sell {:.1}\nCount {}\nAvg print ${:.0}\nBaseline {}",
            self.config.window_secs,
            point.total(),
            point.buy,
            point.sell,
            point.count,
            point.average_notional,
            point.baseline.map_or_else(
                || "warming up".into(),
                |value| format!("{value:.1} prints/s")
            )
        )))
    }
    fn is_point_valid(&self, point: &PacePoint) -> bool {
        point.complete
    }
    fn invalid_point_message(&self) -> Option<&str> {
        Some("Trade Pace: warming up or an incomplete venue stream")
    }
}

impl PacePlot {
    fn draw_series<S: Series<Y = PacePoint>>(
        &self,
        frame: &mut canvas::Frame,
        chart: &ViewState,
        theme: &Theme,
        series: &S,
        range: RangeInclusive<u64>,
        scale: &YScale,
        interval: u64,
    ) {
        let bars = self.bars(interval).bar_width_factor(match chart.basis {
            data::chart::Basis::Time(_) => 0.9,
            data::chart::Basis::Tick(_) => 0.9 * interval as f32,
        });
        bars.draw(frame, chart, theme, series, range.clone(), scale);
        // Dash lengths are measured before the canvas transform. Keep their screen spacing
        // fixed, and stroke whole covered runs so subpixel samples cannot restart the pattern.
        let dash = baseline_dash(chart.scaling);
        let [outline, stroke] = baseline_strokes(theme, &dash);
        for_each_baseline_path(series, range, chart, scale, interval, |path| {
            frame.stroke(&path, outline);
            frame.stroke(&path, stroke);
        });
    }
}

fn baseline_color(theme: &Theme) -> iced::Color {
    theme.extended_palette().background.base.text
}

fn baseline_dash(scaling: f32) -> [f32; 2] {
    let scaling = scaling.max(0.001);
    [2.0 / scaling, 4.0 / scaling]
}

fn baseline_strokes<'a>(theme: &Theme, dash: &'a [f32]) -> [canvas::Stroke<'a>; 2] {
    let stroke = canvas::Stroke {
        width: 1.75,
        line_cap: canvas::LineCap::Round,
        line_dash: canvas::LineDash {
            segments: dash,
            offset: 0,
        },
        ..canvas::Stroke::default()
    }
    .with_color(baseline_color(theme));
    [
        canvas::Stroke {
            width: 3.5,
            ..stroke
        }
        .with_color(theme.extended_palette().background.base.color),
        stroke,
    ]
}

/// One path per covered run. The renderer resets dashing on every stroke, not every sample.
/// Separate paths also prevent a dashed stroke from joining across missing venue coverage.
fn for_each_baseline_path<S: Series<Y = PacePoint>>(
    series: &S,
    range: RangeInclusive<u64>,
    chart: &ViewState,
    scale: &YScale,
    interval: u64,
    mut emit: impl FnMut(canvas::Path),
) {
    let mut builder = canvas::path::Builder::new();
    let mut samples = 0;
    let mut previous = None;
    let mut finish = |builder: &mut canvas::path::Builder, samples: &mut usize| {
        let run = std::mem::replace(builder, canvas::path::Builder::new());
        if *samples > 1 {
            emit(run.build());
        }
        *samples = 0;
    };
    series.for_each_in(range, |x, point| {
        let Some(baseline) = point
            .baseline
            .filter(|value| point.complete && value.is_finite())
        else {
            if previous.take().is_some() {
                finish(&mut builder, &mut samples);
            }
            return;
        };
        let current = Point::new(chart.interval_to_x(x), scale.to_y(baseline));
        if previous.is_some_and(|last_x| x.abs_diff(last_x) <= interval) {
            builder.line_to(current);
        } else {
            finish(&mut builder, &mut samples);
            builder.move_to(current);
        }
        samples += 1;
        previous = Some(x);
    });
    finish(&mut builder, &mut samples);
}

/// Average rates per screen pixel, rather than summing them into a false spike.
/// Only geometry uses this projection; the crosshair reads the original second.
fn pixel_series<S: Series<Y = PacePoint>>(
    series: &S,
    range: RangeInclusive<u64>,
    interval: u64,
) -> BTreeMap<u64, PacePoint> {
    let mut bins: BTreeMap<u64, (PacePoint, u32)> = BTreeMap::new();
    series.for_each_in(range, |x, point| {
        let key = x / interval * interval;
        let (sum, count) = bins.entry(key).or_insert((
            PacePoint {
                complete: true,
                baseline: Some(0.0),
                ..PacePoint::default()
            },
            0,
        ));
        sum.buy += point.buy;
        sum.sell += point.sell;
        sum.complete &= point.complete;
        sum.baseline = sum.baseline.zip(point.baseline).map(|(a, b)| a + b);
        *count += 1;
    });
    bins.into_iter()
        .map(|(x, (mut point, count))| {
            point.buy /= count as f32;
            point.sell /= count as f32;
            point.baseline = point.baseline.map(|sum| sum / count as f32);
            (x, point)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_chart(scaling: f32) -> ViewState {
        use exchange::{Ticker, adapter::Exchange, unit::PriceStep};
        let mut chart = ViewState::new(
            data::chart::Basis::Time(exchange::Timeframe::M1),
            PriceStep {
                units: exchange::unit::Price::from_f32(0.1).units,
            },
            1,
            TickerInfo::new(
                Ticker::new("BTCUSDT", Exchange::BinanceLinear),
                0.1,
                0.001,
                None,
            ),
            data::chart::ViewConfig::default(),
            60.0,
            10.0,
        );
        chart.scaling = scaling;
        chart
    }

    fn native_baseline_meshes(
        points: &BTreeMap<u64, PacePoint>,
        chart: &ViewState,
        theme: &Theme,
        interval: u64,
    ) -> (usize, Vec<iced_wgpu::graphics::Mesh>) {
        use iced_wgpu::graphics::geometry::frame::Backend;
        let mut frame = iced_wgpu::geometry::Frame::new(iced::Rectangle::with_size(
            iced::Size::new(5_000.0, 160.0),
        ));
        frame.scale(chart.scaling);
        let scale = YScale {
            min: 0.0,
            max: 100.0,
            px_height: 160.0 / chart.scaling,
        };
        let dash = baseline_dash(chart.scaling);
        let [outline, stroke] = baseline_strokes(theme, &dash);
        let mut paths = 0;
        for_each_baseline_path(&points, 0..=u64::MAX, chart, &scale, interval, |path| {
            paths += 1;
            frame.stroke(&path, outline);
            frame.stroke(&path, stroke);
        });
        let iced_wgpu::geometry::Geometry::Live { meshes, .. } = frame.into_geometry() else {
            panic!("Expected uncached native geometry");
        };
        (paths, meshes)
    }

    fn foreground_triangles(
        meshes: &[iced_wgpu::graphics::Mesh],
        theme: &Theme,
    ) -> Vec<[[f32; 2]; 3]> {
        let foreground = iced_wgpu::graphics::color::pack(baseline_color(theme));
        meshes
            .iter()
            .flat_map(|mesh| {
                let iced_wgpu::graphics::Mesh::Solid { buffers, .. } = mesh else {
                    panic!("Expected solid baseline geometry");
                };
                buffers.indices.chunks_exact(3).filter_map(|triangle| {
                    let vertices =
                        [0, 1, 2].map(|index| &buffers.vertices[triangle[index] as usize]);
                    (vertices[0].color == foreground).then(|| vertices.map(|v| v.position))
                })
            })
            .collect()
    }

    #[test]
    fn baseline_geometry_remains_visible_and_dotted_at_subpixel_and_zoomed_scales() {
        let points: BTreeMap<_, _> = (0..=1_200)
            .map(|second| {
                (
                    second * 1_000,
                    PacePoint {
                        baseline: Some(56.0),
                        complete: true,
                        ..PacePoint::default()
                    },
                )
            })
            .collect();
        let theme = Theme::Dark;
        for scaling in [0.1, 0.5, 1.0, 2.0, 4.0] {
            let chart = baseline_chart(scaling);
            let (paths, meshes) = native_baseline_meshes(&points, &chart, &theme, 1_000);
            assert_eq!(paths, 1);
            let triangles = foreground_triangles(&meshes, &theme);
            assert!(!triangles.is_empty(), "No baseline at scale {scaling}");
            let mut left = f32::INFINITY;
            let mut right = f32::NEG_INFINITY;
            for triangle in triangles {
                let min_x = triangle.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min);
                let max_x = triangle
                    .iter()
                    .map(|p| p[0])
                    .fold(f32::NEG_INFINITY, f32::max);
                let min_y = triangle.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
                let max_y = triangle
                    .iter()
                    .map(|p| p[1])
                    .fold(f32::NEG_INFINITY, f32::max);
                // Dots must remain separate and retain their screen width at every zoom.
                assert!(max_x - min_x <= 3.76);
                assert!(max_y - min_y <= 1.76);
                left = left.min(min_x);
                right = right.max(max_x);
            }
            assert!(right - left > 100.0, "Baseline vanished at scale {scaling}");
        }
    }

    #[test]
    fn baseline_geometry_does_not_join_warmup_incomplete_or_missing_intervals() {
        let points: BTreeMap<_, _> = (0..=360)
            .filter(|second| !(240..280).contains(second))
            .map(|second| {
                (
                    second * 1_000,
                    PacePoint {
                        baseline: (!(80..120).contains(&second)).then_some(56.0),
                        complete: !(160..200).contains(&second),
                        ..PacePoint::default()
                    },
                )
            })
            .collect();
        let theme = Theme::Light;
        for tick_basis in [false, true] {
            let mut chart = baseline_chart(1.0);
            let (points, interval) = if tick_basis {
                chart.basis = data::chart::Basis::Tick(data::aggr::TickCount(1));
                chart.cell_width = 1.0;
                (
                    points
                        .iter()
                        .map(|(x, point)| (x / 1_000, *point))
                        .collect(),
                    1,
                )
            } else {
                (points.clone(), 1_000)
            };
            let (paths, meshes) = native_baseline_meshes(&points, &chart, &theme, interval);
            assert_eq!(paths, 4);
            let triangles = foreground_triangles(&meshes, &theme);
            assert!(!triangles.is_empty());
            for triangle in triangles {
                let center = triangle.iter().map(|p| p[0].abs()).sum::<f32>() / 3.0;
                for gap in [81.0..119.0, 161.0..199.0, 241.0..279.0] {
                    assert!(!gap.contains(&center), "Baseline crossed a coverage gap");
                }
            }
        }
    }

    #[test]
    fn baseline_uses_readable_theme_ink_and_a_contrasting_outline() {
        let default_theme: Theme = data::config::theme::Theme::default().into();
        let mut screenshot_palette = default_theme.palette();
        screenshot_palette.background = iced::Color::from_rgb(0.08216931, 0.08976005, 0.15175098);
        let screenshot_theme = Theme::custom("Screenshot fixture", screenshot_palette);
        for theme in [Theme::Light, Theme::Dark, screenshot_theme] {
            let palette = theme.extended_palette();
            let strokes = baseline_strokes(&theme, &[2.0, 4.0]);
            assert_eq!(baseline_color(&theme), palette.background.base.text);
            assert!(
                data::config::theme::contrast_ratio(
                    baseline_color(&theme),
                    palette.background.base.color
                ) >= 4.5
            );
            assert!(strokes[0].width > strokes[1].width);
            assert_eq!(baseline_color(&theme).a, 1.0);
        }
    }

    #[test]
    #[ignore = "explicit native baseline geometry cost measurement"]
    fn trade_pace_baseline_geometry_benchmark() {
        let points: BTreeMap<_, _> = (0..14_400)
            .map(|second| {
                (
                    second * 1_000,
                    PacePoint {
                        baseline: Some(40.0 + 20.0 * (second as f32 / 600.0).sin()),
                        complete: true,
                        ..PacePoint::default()
                    },
                )
            })
            .collect();
        let projected = pixel_series(&(&points), 0..=14_400_000, 15_000);
        assert!(projected.len() <= 961);
        let mut chart = baseline_chart(1.0);
        chart.cell_width = 4.0;
        let theme: Theme = data::config::theme::Theme::default().into();
        let mut timings = Vec::new();
        let mut indices = 0;
        for _ in 0..51 {
            let start = std::time::Instant::now();
            let (paths, meshes) = native_baseline_meshes(&projected, &chart, &theme, 15_000);
            timings.push(start.elapsed());
            assert_eq!(paths, 1);
            indices = meshes
                .iter()
                .map(|mesh| mesh.indices().len())
                .sum::<usize>();
            std::hint::black_box(meshes);
        }
        timings.sort();
        println!(
            "Trade Pace baseline native geometry: 14,400 seconds -> {} pixel samples; one covered path, outline + foreground; {} indices; median {:?} per cached one-second update",
            projected.len(),
            indices,
            timings[25]
        );
    }

    #[test]
    fn tick_and_renko_projection_uses_seconds_and_preserves_stream_gaps() {
        use data::aggr::{TickCount, ticks::TickAggr};
        use exchange::unit::{Price, PriceStep, Qty};
        let mut indicator = TradePaceIndicator::new();
        indicator.pace = TradePace::new(Config::default(), UnixMs::new(0));
        indicator.pace.set_connected(true, UnixMs::new(0));
        for second in 1..=90 {
            if second == 51 {
                indicator.pace.set_connected(false, UnixMs::new(51_000));
            } else if second == 53 {
                indicator.pace.set_connected(true, UnixMs::new(53_000));
            }
            indicator.pace.insert(
                &[Trade {
                    time: UnixMs::new(second * 1_000 + 100),
                    is_sell: false,
                    price: Price::from_f64(100.0),
                    qty: Qty::from_f64(1.0),
                }],
                false,
                UnixMs::new(second * 1_000 + 500),
            );
            indicator.pace.publish(UnixMs::new((second + 1) * 1_000));
        }
        let trades = [1_600, 16_500, 20_000, 52_000, 54_000, 80_000].map(|time| Trade {
            time: UnixMs::new(time),
            is_sell: false,
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(1.0),
        });
        let source = PlotData::TickBased(TickAggr::new(
            TickCount(1),
            PriceStep {
                units: Price::from_f64(0.1).units,
            },
            &trades,
        ));
        assert!(indicator.maintain_trade_pace(UnixMs::new(92_000), &source));
        let BasisSeries::Tick(points) = &indicator.tick_data else {
            panic!("tick projection");
        };
        // Irregular brick lifetimes and print counts do not become the TPS denominator.
        assert_eq!(points[&0].time, UnixMs::new(16_000));
        assert_eq!(points[&0].total(), 1.0);
        assert_eq!(points[&1].total(), 1.0);
        assert!(!points[&2].complete);
        assert!(!points[&3].complete);
        assert!(points[&4].complete);
        assert_eq!(points[&5].time, UnixMs::new(92_000));
        let retained = indicator.pace.points().len();
        indicator.on_basis_change(&source);
        assert!(matches!(&indicator.tick_data, BasisSeries::Tick(points) if points.is_empty()));
        assert_eq!(indicator.pace.points().len(), retained);
        assert!(indicator.maintain_trade_pace(UnixMs::new(93_000), &source));
        assert!(matches!(&indicator.tick_data, BasisSeries::Tick(points) if points.len() == 6));
    }

    #[test]
    fn projection_does_not_carry_an_old_rate_into_uncovered_history() {
        use data::aggr::{TickCount, ticks::TickAggr};
        use exchange::unit::{Price, PriceStep, Qty};
        let mut indicator = TradePaceIndicator::new();
        indicator.pace = TradePace::new(Config::default(), UnixMs::new(0));
        indicator.pace.set_connected(true, UnixMs::new(0));
        indicator.pace.publish(UnixMs::new(16_000));
        let trades = [1_000, 2_000, 17_000].map(|time| Trade {
            time: UnixMs::new(time),
            is_sell: false,
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(1.0),
        });
        let source = PlotData::TickBased(TickAggr::new(
            TickCount(1),
            PriceStep {
                units: Price::from_f64(0.1).units,
            },
            &trades,
        ));
        indicator.maintain_trade_pace(UnixMs::new(18_000), &source);
        let BasisSeries::Tick(points) = &indicator.tick_data else {
            panic!("tick projection");
        };
        assert!(!points.contains_key(&0));
        assert_eq!(points[&1].time, UnixMs::new(17_000));
        assert_eq!(points[&2].time, UnixMs::new(18_000));
    }

    #[test]
    #[ignore = "explicit bounded history projection measurement"]
    fn trade_pace_full_history_projection_benchmark() {
        use data::aggr::{TickCount, ticks::TickAggr};
        use exchange::unit::{Price, PriceStep, Qty};
        let mut indicator = TradePaceIndicator::new();
        indicator.pace = TradePace::new(Config::default(), UnixMs::new(0));
        indicator.pace.set_connected(true, UnixMs::new(0));
        indicator.pace.publish(UnixMs::new(1_000));
        indicator.pace.publish(UnixMs::new(20_000_000));
        let trades: Vec<_> = (1..=20_000)
            .map(|second| Trade {
                time: UnixMs::new(second * 1_000),
                is_sell: false,
                price: Price::from_f64(100.0),
                qty: Qty::from_f64(1.0),
            })
            .collect();
        let source = PlotData::TickBased(TickAggr::new(
            TickCount(1),
            PriceStep {
                units: Price::from_f64(0.1).units,
            },
            &trades,
        ));
        let mut projection = Vec::new();
        let mut geometry = Vec::new();
        for second in 20_001..=20_015 {
            let start = std::time::Instant::now();
            assert!(indicator.maintain_trade_pace(UnixMs::new(second * 1_000), &source));
            projection.push(start.elapsed());
            let BasisSeries::Tick(points) = &indicator.tick_data else {
                panic!("tick projection");
            };
            assert!(points.len() <= RETENTION_SECS as usize);
            let start = std::time::Instant::now();
            let pixels = pixel_series(&points, 0..=20_000, 15);
            geometry.push(start.elapsed());
            assert!(pixels.len() <= 961);
            std::hint::black_box(pixels);
        }
        projection.sort();
        geometry.sort();
        println!(
            "Trade Pace full history: 20,000 primary tick bars; at most 14,400 pace samples; 1Hz publication + tick projection median {:?}; pixel projection (<1000 bins) median {:?}",
            projection[7], geometry[7]
        );
    }

    #[test]
    fn counts_only_selected_live_sources_and_rejects_backfill() {
        use exchange::{
            Ticker,
            adapter::Exchange,
            unit::{Price, Qty},
        };
        let source = |venue| TickerInfo::new(Ticker::new("BTCUSDT", venue), 0.1, 0.001, None);
        let binance = source(Exchange::BinanceLinear);
        let bybit = source(Exchange::BybitLinear);
        let other = source(Exchange::OkexLinear);
        let now = UnixMs::now();
        let start = UnixMs::new(now.as_u64().saturating_sub(60_000));
        let mut indicator = TradePaceIndicator::new();
        indicator.configure_trade_pace(&[binance, bybit], Config::default());
        indicator.pace = TradePace::new(Config::default(), start);
        indicator.trade_pace_connection(binance, true, start);
        indicator.trade_pace_connection(bybit, true, start);
        let trade = Trade {
            time: now,
            is_sell: false,
            price: Price::from_f64(100.0),
            qty: Qty::from_f64(1.0),
        };
        indicator.on_source_trades(binance, &[trade], false);
        indicator.on_source_trades(bybit, &[trade], false);
        indicator.on_source_trades(other, &[trade], false);
        indicator.on_source_trades(binance, &[trade], true);
        indicator.pace.publish(UnixMs::new(now.as_u64() + 1_000));
        assert_eq!(indicator.pace.points().last_key_value().unwrap().1.count, 2);
    }

    #[test]
    fn pixel_projection_averages_rates_and_keeps_incomplete_bins_hidden() {
        let mut series = BTreeMap::new();
        for second in 0..1_000 {
            series.insert(
                second * 1_000,
                PacePoint {
                    buy: 2.0,
                    sell: 1.0,
                    baseline: Some(4.0),
                    complete: second != 155,
                    ..PacePoint::default()
                },
            );
        }
        let projected = pixel_series(&(&series), 0..=999_000, 10_000);
        assert_eq!(projected.len(), 100);
        assert_eq!(projected[&0].total(), 3.0);
        assert_eq!(projected[&0].baseline, Some(4.0));
        assert!(!projected[&150_000].complete);
        assert!(projected[&160_000].complete);
    }
}
