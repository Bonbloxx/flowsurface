//! Render the real native pane picker, settings and chart canvases with an
//! offline synthetic tape. No subscriptions, profile reads or market requests.
//! cargo run --example trev_native_qa -- target/trev-native-qa
#![allow(dead_code, unused_imports)]

mod audio;
mod chart;
mod connector;
mod layout;
mod logger;
mod modal;
mod notify;
mod screen;
mod style;
mod version;
mod widget;
mod window;

use chart::kline::KlineChart;
use data::{
    aggr::ticks::TickAggr,
    chart::{Basis, KlineChartKind, ViewConfig, trev},
    layout::pane::ContentKind,
};
use exchange::{
    Ticker, TickerInfo, Trade, UnixMs,
    adapter::{AdapterHandles, Exchange},
    unit::{Price, PriceStep, Qty},
};
use iced::widget::tooltip::Position as TooltipPosition;
use iced::{
    Element, Task,
    widget::{column, pane_grid, text},
};
use screen::dashboard;
use screen::dashboard::{
    pane::{self, Content, Event},
    tickers_table::TickersTable,
};
use std::{
    borrow::Cow,
    path::PathBuf,
    time::{Duration, Instant},
};
use widget::tooltip;

const SHOTS: [&str; 12] = [
    "picker",
    "trev-candles",
    "settings",
    "bid-ask",
    "volume",
    "delta",
    "row-aggregation",
    "restored",
    "candles-regression",
    "footprint-regression",
    "renko-regression",
    "tpo-regression",
];

struct Preview {
    panes: pane_grid::State<pane::State>,
    pane: pane_grid::Pane,
    window: window::Window,
    table: TickersTable,
    stage: usize,
    output: PathBuf,
}

#[derive(Debug, Clone)]
enum Message {
    Capture,
    Captured(iced::window::Screenshot),
    Pane(pane::Message),
}

fn source() -> TickerInfo {
    TickerInfo::new(
        Ticker::new("BTCUSDT", Exchange::BinanceLinear),
        0.1,
        0.001,
        None,
    )
}

fn tape() -> Vec<Trade> {
    let now = UnixMs::now().as_u64();
    (0..900)
        .map(|i| {
            let phase = i % 180;
            let offset = if phase < 90 {
                phase as f64
            } else {
                (180 - phase) as f64
            };
            Trade {
                time: UnixMs::new(now - (900 - i) * 1000),
                price: Price::from_f64(100_000. + offset),
                qty: Qty::from_f64(0.01 * (1 + i % 15) as f64),
                is_sell: i % 3 == 0,
            }
        })
        .collect()
}

impl Preview {
    fn boot() -> (Self, Task<Message>) {
        let output = std::env::args()
            .nth(1)
            .map_or_else(|| PathBuf::from("target/trev-native-qa"), PathBuf::from);
        std::fs::create_dir_all(&output).unwrap();
        let (id, open) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(1280., 900.),
            visible: true,
            position: iced::window::Position::Specific(iced::Point::new(-5000., 0.)),
            ..Default::default()
        });
        let (panes, pane) = pane_grid::State::new(pane::State::default());
        let handles = AdapterHandles::spawn_venues(&reqwest::Client::new(), [], None);
        let (table, _) = TickersTable::new(handles);
        (
            Self {
                panes,
                pane,
                window: window::Window::new(id),
                table,
                stage: 0,
                output,
            },
            open.then(|_| Self::delay()),
        )
    }

    fn delay() -> Task<Message> {
        Task::perform(
            async {
                tokio::time::sleep(Duration::from_millis(500)).await;
            },
            |_| Message::Capture,
        )
    }

    fn seed(&mut self, kind: ContentKind) {
        let state = self.panes.get_mut(self.pane).unwrap();
        state.set_content_and_streams(vec![source()], kind);
        let Content::Kline {
            chart: Some(chart), ..
        } = &mut state.content
        else {
            panic!("chart");
        };
        let trades = tape();
        let step = PriceStep::from(source().min_ticksize);
        match chart.kind().clone() {
            KlineChartKind::Trev { config } => {
                let aggr = TickAggr::new_trev(config, step, &trades);
                chart.restore_trev_snapshot(&aggr.trev_snapshot(source().ticker).unwrap());
            }
            KlineChartKind::Renko { config } => {
                let aggr = TickAggr::new_renko(config, step, &trades);
                chart.restore_renko_snapshot(&aggr.renko_snapshot(source().ticker).unwrap());
            }
            kind => {
                let klines = trades
                    .chunks(60)
                    .map(|trades| exchange::Kline {
                        time: trades[0].time.floor_to(exchange::Timeframe::M1),
                        open: trades[0].price,
                        close: trades.last().unwrap().price,
                        high: trades.iter().map(|t| t.price).max().unwrap(),
                        low: trades.iter().map(|t| t.price).min().unwrap(),
                        volume: trades
                            .iter()
                            .fold(exchange::Volume::empty_buy_sell(), |v, t| {
                                v.add_trade_qty(t.is_sell, t.qty)
                            }),
                    })
                    .collect::<Vec<_>>();
                **chart = KlineChart::new(
                    ViewConfig::default(),
                    Basis::Time(exchange::Timeframe::M1),
                    step,
                    &klines,
                    trades.clone(),
                    &[],
                    source(),
                    &kind,
                    None,
                );
            }
        }
        let last = trades.last().unwrap();
        chart.insert_trades(
            source(),
            &[Trade {
                time: last.time.saturating_add(1),
                ..*last
            }],
        );
        chart.invalidate(Some(Instant::now()));
    }

    fn advance(&mut self) {
        match self.stage {
            1 => self.seed(ContentKind::TrevChart),
            2 => {
                self.panes
                    .get_mut(self.pane)
                    .unwrap()
                    .update(Event::ShowModal(modal::pane::Modal::Settings));
            }
            3..=6 => {
                let state = self.panes.get_mut(self.pane).unwrap();
                state.update(Event::HideModal);
                let Content::Kline {
                    chart: Some(chart), ..
                } = &state.content
                else {
                    panic!("chart");
                };
                let KlineChartKind::Trev { config } = *chart.kind() else {
                    panic!("TRev");
                };
                let display = match self.stage {
                    3 => trev::Display::BidAsk,
                    4 => trev::Display::Volume,
                    _ => trev::Display::Delta,
                };
                state.update(Event::TrevConfigChanged(trev::Config {
                    display,
                    ticks_per_row: if self.stage == 6 { 50 } else { 20 },
                    ..config
                }));
            }
            7 => {
                let state = self.panes.get(self.pane).unwrap();
                let saved = data::Pane::from(state);
                let json = serde_json::to_string_pretty(&saved).unwrap();
                std::fs::write(self.output.join("pane.json"), &json).unwrap();
                let pane_grid::Configuration::Pane(mut restored) =
                    layout::configuration(serde_json::from_str(&json).unwrap())
                else {
                    panic!("pane");
                };
                restored.set_content_and_streams(vec![source()], ContentKind::TrevChart);
                let before = serde_json::to_value(data::Pane::from(state)).unwrap();
                let after = serde_json::to_value(data::Pane::from(&restored)).unwrap();
                let before = &before["KlineChart"]["trev_state"];
                let after = &after["KlineChart"]["trev_state"];
                assert!(
                    before["bars"] == after["bars"],
                    "restart must preserve all OHLC, quantities and footprint cells"
                );
                for key in ["direction", "last_time", "last_trades"] {
                    assert!(
                        before["state"][key] == after["state"][key],
                        "restart must preserve continuation state: {key}"
                    );
                }
                *self.panes.get_mut(self.pane).unwrap() = restored;
            }
            8 => self.seed(ContentKind::CandlestickChart),
            9 => self.seed(ContentKind::FootprintChart),
            10 => self.seed(ContentKind::RenkoChart),
            11 => self.seed(ContentKind::TpoChart),
            _ => {}
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Capture => iced::window::screenshot(self.window.id).map(Message::Captured),
            Message::Captured(shot) => {
                let path = self.output.join(SHOTS[self.stage.min(SHOTS.len() - 1)]);
                std::fs::write(path.with_extension("rgba"), shot.as_ref()).unwrap();
                std::fs::write(path.with_extension("json"),serde_json::json!({"width":shot.size.width,"height":shot.size.height,"scale":shot.scale_factor}).to_string()).unwrap();
                println!("Captured {}", SHOTS[self.stage.min(SHOTS.len() - 1)]);
                self.stage += 1;
                if self.stage == SHOTS.len() {
                    return iced::exit();
                }
                self.advance();
                Self::delay()
            }
            Message::Pane(pane::Message::PaneEvent(_, event)) => {
                self.panes.get_mut(self.pane).unwrap().update(event);
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn view(&self, window: iced::window::Id) -> Element<'_, Message> {
        column![
            text(format!(
                "Native offline verification: {}",
                SHOTS[self.stage.min(SHOTS.len() - 1)]
            ))
            .size(14),
            Element::from(pane_grid::PaneGrid::new(&self.panes, |pane, state, _| {
                state.view(
                    pane,
                    1,
                    true,
                    false,
                    window,
                    &self.window,
                    Default::default(),
                    &self.table,
                )
            }))
            .map(Message::Pane)
        ]
        .into()
    }
}

fn main() -> iced::Result {
    let qa_root = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("target/trev-native-qa"), PathBuf::from);
    let profile = std::env::current_dir()
        .unwrap()
        .join(qa_root)
        .join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    // The standalone preview has not started any runtime or threads yet.
    unsafe {
        std::env::set_var("FLOWSURFACE_DATA_PATH", &profile);
    }

    iced::daemon(Preview::boot, Preview::update, Preview::view)
        .settings(iced::Settings {
            antialiasing: true,
            fonts: vec![
                Cow::Borrowed(style::AZERET_MONO_BYTES),
                Cow::Borrowed(style::ICONS_BYTES),
            ],
            ..Default::default()
        })
        .title("TRev native verification")
        .theme(|_: &Preview, _| data::Theme::default().0)
        .run()
}
