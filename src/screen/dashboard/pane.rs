use crate::{
    chart::{
        self, comparison::ComparisonChart, footprint_history::FootprintHistory,
        heatmap::HeatmapChart, kline::KlineChart,
    },
    connector::{
        ResolvedStream,
        fetcher::{FetchSpec, InfoKind, TradeFetchMode, trade_fetch_mode},
    },
    modal::{
        self, ModifierKind,
        pane::{
            Modal,
            mini_tickers_list::MiniPanel,
            settings::{
                comparison_cfg_view, footprint_history_cfg_view, heatmap_cfg_view,
                heatmap_shader_cfg_view, kline_cfg_view,
            },
            stack_modal,
        },
    },
    screen::dashboard::{
        panel::{self, ladder::Ladder, timeandsales::TimeAndSales},
        tickers_table::TickersTable,
    },
    style::{self, Icon, icon_text},
    widget::{
        self, button_with_tooltip, chart::heatmap::HeatmapShader, column_drag, link_group_button,
        toast::Toast,
    },
    window::{self, Window},
};
use data::{
    UserTimezone,
    aggregation::{AggregateFeedId, ResolvedFeed},
    chart::{
        Basis, ViewConfig,
        heatmap::HeatmapStudy,
        indicator::{HeatmapIndicator, Indicator, KlineIndicator, UiIndicator},
    },
    layout::pane::{ContentKind, LinkGroup, PaneSetup, Settings, VisualConfig},
    stream::{PersistDepth, PersistStreamKind},
};
use exchange::{
    Kline, OpenInterest, StreamPairKind, TickMultiplier, TickerInfo, Timeframe,
    adapter::{MarketKind, StreamKind, StreamTicksize, Venue},
    unit::PriceStep,
};
use iced::{
    Alignment, Element, Length, Renderer, Theme, padding,
    widget::{button, center, column, container, pane_grid, pick_list, row, text, tooltip},
};
use std::time::Instant;

const INITIALIZING_TICK_INTERVAL_MS: u64 = 100;
const IDLE_TICK_INTERVAL_MS: u64 = 1000;

#[derive(Debug, Clone)]
pub enum Effect {
    RefreshStreams,
    RequestFetch(Vec<FetchSpec>),
    SwitchTickersInGroup(TickerInfo),
    FocusWidget(iced::widget::Id),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub enum Status {
    #[default]
    Ready,
    Loading(InfoKind),
    Stale(String),
}

pub enum Action {
    Chart(chart::Action),
    Panel(panel::Action),
    ResolveStreams(Vec<PersistStreamKind>),
    ResolveContent,
}

fn has_trade_history_indicator(indicators: &[KlineIndicator]) -> bool {
    indicators
        .iter()
        .copied()
        .any(KlineIndicator::needs_trade_history)
}

fn restore_configured_trade_history_streams(
    content: &Content,
    settings: &Settings,
    streams: &mut Vec<PersistStreamKind>,
) {
    let uses_trade_history = match content {
        Content::Kline { indicators, .. } => has_trade_history_indicator(indicators),
        Content::FootprintHistory(_) => true,
        _ => false,
    };
    if !uses_trade_history {
        return;
    }

    let Some(selected) = settings.footprint_history_sources.as_deref() else {
        return;
    };
    let active = if settings.footprint_history_aggregate {
        selected
    } else {
        selected.get(..1).unwrap_or(&[])
    };

    for ticker in active {
        let already_restored = streams.iter().any(|stream| match stream {
            PersistStreamKind::Trades { ticker: existing } => existing.same_market(ticker),
            PersistStreamKind::AggregateTrades { feed } => feed
                .source_tickers()
                .any(|source| source.same_market(ticker)),
            PersistStreamKind::DepthAndTrades(depth) => depth.ticker.same_market(ticker),
            PersistStreamKind::Kline { .. } | PersistStreamKind::Depth(_) => false,
        });
        if !already_restored {
            streams.push(PersistStreamKind::Trades { ticker: *ticker });
        }
    }
}

fn restore_footprint_kline_stream(
    content: &Content,
    settings: &Settings,
    streams: &mut Vec<PersistStreamKind>,
) {
    let Content::Kline { kind, .. } = content else {
        return;
    };
    if !matches!(kind, data::chart::KlineChartKind::Footprint { .. }) {
        return;
    }
    if streams
        .iter()
        .any(|stream| matches!(stream, PersistStreamKind::Kline { .. }))
    {
        return;
    }
    let timeframe = match settings.selected_basis {
        Some(Basis::Time(tf)) => tf,
        _ => Timeframe::M5,
    };
    let ticker = streams.iter().find_map(|stream| match stream {
        PersistStreamKind::Kline { ticker, .. }
        | PersistStreamKind::Trades { ticker }
        | PersistStreamKind::Depth(PersistDepth { ticker, .. })
        | PersistStreamKind::DepthAndTrades(PersistDepth { ticker, .. }) => Some(*ticker),
        PersistStreamKind::AggregateTrades { feed } => feed.source_tickers().next(),
    });
    if let Some(ticker) = ticker {
        streams.push(PersistStreamKind::Kline { ticker, timeframe });
    }
}

fn restore_configured_visible_trade_streams(
    content: &Content,
    streams: &mut Vec<PersistStreamKind>,
) {
    let Content::Kline { indicators, .. } = content else {
        return;
    };
    if !indicators
        .iter()
        .copied()
        .any(KlineIndicator::needs_visible_trades)
    {
        return;
    }

    let Some(ticker) = streams.iter().find_map(|stream| match stream {
        PersistStreamKind::Kline { ticker, .. }
        | PersistStreamKind::Trades { ticker }
        | PersistStreamKind::DepthAndTrades(PersistDepth { ticker, .. })
        | PersistStreamKind::Depth(PersistDepth { ticker, .. }) => Some(*ticker),
        PersistStreamKind::AggregateTrades { feed } => feed.source_tickers().next(),
    }) else {
        return;
    };

    let already_restored = streams.iter().any(|stream| match stream {
        PersistStreamKind::Trades { ticker: existing } => existing.same_market(&ticker),
        PersistStreamKind::AggregateTrades { feed } => feed
            .source_tickers()
            .any(|source| source.same_market(&ticker)),
        PersistStreamKind::DepthAndTrades(depth) => depth.ticker.same_market(&ticker),
        PersistStreamKind::Kline { .. } | PersistStreamKind::Depth(_) => false,
    });
    if !already_restored {
        streams.push(PersistStreamKind::Trades { ticker });
    }
}

fn has_liquidity_heatmap(indicators: &[KlineIndicator]) -> bool {
    indicators.contains(&KlineIndicator::LiquidityHeatmap)
}

fn restore_configured_liquidity_heatmap_streams(
    content: &Content,
    settings: &Settings,
    streams: &mut Vec<PersistStreamKind>,
) {
    let Content::Kline { indicators, .. } = content else {
        return;
    };
    if !has_liquidity_heatmap(indicators) {
        return;
    }

    let Some(selected) = settings.liquidity_heatmap_sources.as_deref() else {
        return;
    };

    for ticker in selected {
        let already_restored = streams.iter().any(|stream| match stream {
            PersistStreamKind::Depth(depth) | PersistStreamKind::DepthAndTrades(depth) => {
                depth.ticker.same_market(ticker)
            }
            PersistStreamKind::Kline { .. }
            | PersistStreamKind::Trades { .. }
            | PersistStreamKind::AggregateTrades { .. } => false,
        });
        if already_restored {
            continue;
        }

        streams.push(PersistStreamKind::Depth(PersistDepth {
            ticker: *ticker,
            depth_aggr: ticker
                .exchange
                .stream_ticksize(Some(TickMultiplier(1)), TickMultiplier(1)),
            push_freq: exchange::PushFrequency::ServerDefault,
        }));
    }
}

fn default_footprint_history_sources(available: &[TickerInfo]) -> Vec<TickerInfo> {
    let mut selected = match trade_fetch_mode() {
        // Direct Hyperliquid public history is not available. Leaving it off by
        // default keeps all three displayed days comparable and avoids a warning
        // on a fresh Exchange-mode history pane. It remains available to opt in.
        TradeFetchMode::Exchange => available
            .iter()
            .copied()
            .filter(|source| matches!(source.exchange().venue(), Venue::Binance | Venue::Bybit))
            .collect(),
        TradeFetchMode::Off | TradeFetchMode::Server => available.to_vec(),
    };
    if selected.is_empty()
        && let Some(first) = available.first()
    {
        selected.push(*first);
    }
    selected
}

#[derive(Debug, Clone)]
pub enum Message {
    PaneClicked(pane_grid::Pane),
    PaneResized(pane_grid::ResizeEvent),
    PaneDragged(pane_grid::DragEvent),
    ClosePane(pane_grid::Pane),
    SplitPane(pane_grid::Axis, pane_grid::Pane),
    MaximizePane(pane_grid::Pane),
    Restore,
    ReplacePane(pane_grid::Pane),
    Popout,
    Merge,
    SwitchLinkGroup(pane_grid::Pane, Option<LinkGroup>),
    VisualConfigChanged(pane_grid::Pane, VisualConfig, bool),
    PaneEvent(pane_grid::Pane, Event),
}

#[derive(Debug, Clone)]
pub enum Event {
    ShowModal(Modal),
    HideModal,
    ContentSelected(ContentKind),
    ChartInteraction(super::chart::Message),
    PanelInteraction(super::panel::Message),
    ToggleIndicator(UiIndicator, Vec<TickerInfo>),
    DeleteNotification(usize),
    ReorderIndicator(column_drag::DragEvent),
    ClusterKindSelected(data::chart::kline::ClusterKind),
    ClusterScalingSelected(data::chart::kline::ClusterScaling),
    TicksizeSelected(TickMultiplier),
    RenkoConfigChanged(data::chart::kline::RenkoConfig),
    TpoConfigChanged(data::chart::tpo::Config),
    AggregateSourceToggled(TickerInfo, bool),
    FootprintHistoryAggregationToggled(bool),
    FootprintHistorySourceToggled(TickerInfo, bool),
    LiquidityHeatmapSourceToggled(TickerInfo, bool),
    StudyConfigurator(modal::pane::settings::study::StudyMessage),
    StreamModifierChanged(modal::stream::Message),
    ComparisonChartInteraction(super::chart::comparison::Message),
    HeatmapShaderInteraction(crate::widget::chart::heatmap::Message),
    MiniTickersListInteraction(modal::pane::mini_tickers_list::Message),
}

pub struct State {
    id: uuid::Uuid,
    pub modal: Option<Modal>,
    pub content: Content,
    pub settings: Settings,
    pub notifications: Vec<Toast>,
    pub streams: ResolvedStream,
    pub status: Status,
    pub link_group: Option<LinkGroup>,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_config(
        content: Content,
        mut streams: Vec<PersistStreamKind>,
        settings: Settings,
        link_group: Option<LinkGroup>,
    ) -> Self {
        restore_configured_trade_history_streams(&content, &settings, &mut streams);
        restore_configured_visible_trade_streams(&content, &mut streams);
        restore_configured_liquidity_heatmap_streams(&content, &settings, &mut streams);
        restore_footprint_kline_stream(&content, &settings, &mut streams);

        Self {
            content,
            settings,
            streams: ResolvedStream::waiting(streams),
            link_group,
            ..Default::default()
        }
    }

    pub fn stream_pair(&self) -> Option<TickerInfo> {
        self.streams.find_ready_map(|stream| match stream {
            StreamKind::Kline { ticker_info, .. } => Some(*ticker_info),
            StreamKind::Depth { ticker_info, .. } => Some(*ticker_info),
            StreamKind::Trades { ticker_info, .. } => Some(*ticker_info),
        })
    }

    pub fn stream_pair_kind(&self) -> Option<StreamPairKind> {
        let ready_streams = self.streams.ready_iter()?;
        let mut unique = vec![];

        let main_trade_sources = match &self.content {
            Content::Kline {
                chart: Some(chart),
                indicators,
                ..
            } if has_trade_history_indicator(indicators) => {
                let uses_trades = matches!(
                    chart.kind(),
                    data::chart::KlineChartKind::Footprint { .. }
                        | data::chart::KlineChartKind::Renko { .. }
                        | data::chart::KlineChartKind::Tpo { .. }
                ) || matches!(chart.basis(), Basis::Tick(_));
                if !uses_trades {
                    Some(Vec::new())
                } else if matches!(
                    chart.kind(),
                    data::chart::KlineChartKind::Footprint { .. }
                        | data::chart::KlineChartKind::Tpo { .. }
                ) {
                    Some(chart.feed().sources().to_vec())
                } else {
                    Some(vec![chart.feed().primary()])
                }
            }
            _ => None,
        };
        let overlay_depth_sources = match &self.content {
            Content::Kline {
                chart: Some(chart),
                indicators,
                ..
            } if has_liquidity_heatmap(indicators) => {
                Some(chart.liquidity_heatmap_sources().to_vec())
            }
            _ => None,
        };

        for stream in ready_streams {
            if let (Some(main_sources), StreamKind::Trades { ticker_info }) =
                (&main_trade_sources, stream)
                && !main_sources
                    .iter()
                    .any(|source| source.ticker.same_market(&ticker_info.ticker))
            {
                continue;
            }
            if let (Some(sources), StreamKind::Depth { ticker_info, .. }) =
                (&overlay_depth_sources, stream)
                && sources
                    .iter()
                    .any(|source| source.ticker.same_market(&ticker_info.ticker))
            {
                continue;
            }
            let ticker = stream.ticker_info();
            if !unique.contains(&ticker) {
                unique.push(ticker);
            }
        }

        match unique.len() {
            0 => None,
            1 => Some(StreamPairKind::SingleSource(unique[0])),
            _ => Some(StreamPairKind::MultiSource(unique)),
        }
    }

    pub fn set_content_and_streams(
        &mut self,
        tickers: Vec<TickerInfo>,
        kind: ContentKind,
    ) -> Vec<StreamKind> {
        if !(self.content.kind() == kind) {
            self.settings.selected_basis = None;
            self.settings.tick_multiply = None;
        }

        let base_ticker = tickers[0];
        let prev_base_ticker = self.stream_pair();
        let aggregate_feed = if kind.supports_aggregate_feed() {
            self.settings
                .aggregate_feed
                .filter(|feed| {
                    feed.source_tickers()
                        .any(|ticker| ticker == base_ticker.ticker)
                })
                .or_else(|| AggregateFeedId::for_seed_ticker(base_ticker.ticker))
        } else {
            None
        };
        self.settings.aggregate_feed = aggregate_feed;
        if !kind.supports_aggregate_feed() {
            self.settings.aggregate_sources = None;
        }
        let resolved_feed = if let Some(feed) = aggregate_feed {
            ResolvedFeed::aggregated_selected(
                feed,
                base_ticker,
                &tickers,
                self.settings.aggregate_sources.as_deref(),
            )
        } else if kind.supports_aggregate_feed() && tickers.len() > 1 {
            ResolvedFeed::from_sources_selected(
                base_ticker,
                &tickers,
                self.settings.aggregate_sources.as_deref(),
            )
        } else {
            ResolvedFeed::direct(base_ticker)
        };

        let setup_ticker = if matches!(kind, ContentKind::FootprintChart | ContentKind::TpoChart) {
            resolved_feed.primary()
        } else {
            base_ticker
        };
        let derived_plan = PaneSetup::new(
            kind,
            setup_ticker,
            prev_base_ticker,
            self.settings.selected_basis,
            self.settings.tick_multiply,
        );

        self.settings.selected_basis = derived_plan.basis;
        self.settings.tick_multiply = derived_plan.tick_multiplier;

        let (mut content, streams) = {
            let kline_stream = |ti: TickerInfo, tf: Timeframe| StreamKind::Kline {
                ticker_info: ti,
                timeframe: tf,
            };
            let depth_stream = |derived_plan: &PaneSetup| StreamKind::Depth {
                ticker_info: derived_plan.ticker_info,
                depth_aggr: derived_plan.depth_aggr,
                push_freq: derived_plan.push_freq,
            };
            let trades_stream = |derived_plan: &PaneSetup| StreamKind::Trades {
                ticker_info: derived_plan.ticker_info,
            };

            match kind {
                ContentKind::HeatmapChart => {
                    let content = Content::new_heatmap(
                        &self.content,
                        derived_plan.ticker_info,
                        &self.settings,
                        derived_plan.price_step,
                    );

                    let streams = vec![depth_stream(&derived_plan), trades_stream(&derived_plan)];

                    (content, streams)
                }
                ContentKind::FootprintChart => {
                    // Footprints are stored at the feed's base tick; the
                    // tick multiplier is a display-time grouping only.
                    let base_step = resolved_feed.price_step();
                    let mut content = Content::new_kline(
                        kind,
                        &self.content,
                        resolved_feed.primary(),
                        &self.settings,
                        base_step,
                    );
                    if let Content::Kline {
                        chart: Some(chart), ..
                    } = &mut content
                    {
                        chart.set_feed(resolved_feed.clone());
                        chart.change_tick_size(
                            self.settings
                                .tick_multiply
                                .unwrap_or(TickMultiplier(50))
                                .multiply_step(base_step),
                        );
                    }

                    let streams = by_basis_default(
                        derived_plan.basis,
                        Timeframe::M5,
                        |tf| {
                            let mut streams = resolved_feed.trade_streams();
                            streams.push(kline_stream(resolved_feed.primary(), tf));
                            streams
                        },
                        || resolved_feed.trade_streams(),
                    );

                    (content, streams)
                }
                ContentKind::FootprintHistory => {
                    let available = data::aggregation::equivalent_footprint_sources(
                        base_ticker,
                        tickers.iter().copied(),
                    );
                    let selected = self
                        .settings
                        .footprint_history_sources
                        .as_deref()
                        .map(|wanted| {
                            available
                                .iter()
                                .copied()
                                .filter(|source| {
                                    wanted
                                        .iter()
                                        .any(|ticker| ticker.same_market(&source.ticker))
                                })
                                .collect::<Vec<_>>()
                        })
                        .filter(|sources| !sources.is_empty())
                        .unwrap_or_else(|| default_footprint_history_sources(&available));
                    let aggregate = self.settings.footprint_history_sources.is_none()
                        || self.settings.footprint_history_aggregate;
                    self.settings.footprint_history_aggregate = aggregate;
                    self.settings.footprint_history_sources =
                        Some(selected.iter().map(|source| source.ticker).collect());
                    let min_step = selected
                        .iter()
                        .map(|source| PriceStep::from(source.min_ticksize))
                        .min_by_key(|step| step.units)
                        .unwrap_or(derived_plan.price_step);
                    let block_step = self
                        .settings
                        .tick_multiply
                        .unwrap_or(TickMultiplier(1000))
                        .multiply_step(min_step);
                    let history = FootprintHistory::new(selected.clone(), aggregate, block_step);
                    let active = if aggregate {
                        selected
                    } else {
                        selected.into_iter().take(1).collect()
                    };
                    let streams = active
                        .into_iter()
                        .map(|ticker_info| StreamKind::Trades { ticker_info })
                        .collect();

                    (Content::FootprintHistory(Some(history)), streams)
                }
                ContentKind::RenkoChart => {
                    let content = Content::new_kline(
                        kind,
                        &self.content,
                        derived_plan.ticker_info,
                        &self.settings,
                        derived_plan.price_step,
                    );
                    let streams = vec![trades_stream(&derived_plan)];

                    (content, streams)
                }
                ContentKind::TpoChart => {
                    let feed_price_step = resolved_feed.price_step();
                    let mut content = Content::new_kline(
                        kind,
                        &self.content,
                        resolved_feed.primary(),
                        &self.settings,
                        feed_price_step,
                    );
                    if let Content::Kline {
                        chart: Some(chart), ..
                    } = &mut content
                    {
                        chart.set_feed(resolved_feed.clone());
                    }
                    let streams = resolved_feed.trade_streams();

                    (content, streams)
                }
                ContentKind::CandlestickChart => {
                    let content = {
                        let base_ticker = tickers[0];
                        Content::new_kline(
                            kind,
                            &self.content,
                            derived_plan.ticker_info,
                            &self.settings,
                            base_ticker.min_ticksize.into(),
                        )
                    };

                    let time_basis_stream = |tf| vec![kline_stream(derived_plan.ticker_info, tf)];
                    let tick_basis_stream = || {
                        let depth_aggr = derived_plan
                            .ticker_info
                            .exchange()
                            .stream_ticksize(None, TickMultiplier(50));
                        let temp = PaneSetup {
                            depth_aggr,
                            ..derived_plan
                        };
                        vec![trades_stream(&temp)]
                    };

                    let streams = by_basis_default(
                        derived_plan.basis,
                        Timeframe::M15,
                        time_basis_stream,
                        tick_basis_stream,
                    );

                    (content, streams)
                }
                ContentKind::TimeAndSales => {
                    let config = self
                        .settings
                        .visual_config
                        .clone()
                        .and_then(|cfg| cfg.time_and_sales());
                    let content = Content::TimeAndSales(Some(TimeAndSales::new(
                        config,
                        derived_plan.ticker_info,
                    )));

                    let temp = PaneSetup {
                        push_freq: exchange::PushFrequency::ServerDefault,
                        ..derived_plan
                    };

                    let streams = vec![trades_stream(&temp)];

                    (content, streams)
                }
                ContentKind::Ladder => {
                    let config = self
                        .settings
                        .visual_config
                        .clone()
                        .and_then(|cfg| cfg.ladder());
                    let content = Content::Ladder(Some(Ladder::new(
                        config,
                        derived_plan.ticker_info,
                        derived_plan.price_step,
                    )));

                    let streams = vec![depth_stream(&derived_plan), trades_stream(&derived_plan)];

                    (content, streams)
                }
                ContentKind::ComparisonChart => {
                    let config = self
                        .settings
                        .visual_config
                        .clone()
                        .and_then(|cfg| cfg.comparison());

                    let timeframe = {
                        let supports = |tf| {
                            tickers
                                .iter()
                                .all(|ti| ti.exchange().supports_kline_timeframe(tf))
                        };

                        if let Some(tf) = derived_plan.basis.and_then(|basis| match basis {
                            Basis::Time(tf) => Some(tf),
                            Basis::Tick(_) => None,
                        }) && supports(tf)
                        {
                            tf
                        } else {
                            let fallback = Timeframe::M15;
                            if supports(fallback) {
                                fallback
                            } else {
                                Timeframe::KLINE
                                    .iter()
                                    .copied()
                                    .find(|tf| supports(*tf))
                                    .unwrap_or(fallback)
                            }
                        }
                    };

                    let basis = Basis::Time(timeframe);
                    self.settings.selected_basis = Some(basis);
                    let content =
                        Content::Comparison(Some(ComparisonChart::new(basis, &tickers, config)));

                    let streams = tickers
                        .iter()
                        .copied()
                        .map(|ti| kline_stream(ti, timeframe))
                        .collect();

                    (content, streams)
                }
                ContentKind::ShaderHeatmap => {
                    let basis = derived_plan
                        .basis
                        .unwrap_or(Basis::default_heatmap_time(Some(derived_plan.ticker_info)));

                    let (studies, indicators) = if let Content::ShaderHeatmap {
                        chart,
                        indicators,
                        studies,
                    } = &self.content
                    {
                        (
                            chart
                                .as_ref()
                                .map_or(studies.clone(), |c| c.studies.clone()),
                            indicators.clone(),
                        )
                    } else {
                        (
                            vec![HeatmapStudy::VolumeProfile(
                                data::chart::heatmap::ProfileKind::default(),
                            )],
                            vec![HeatmapIndicator::Volume],
                        )
                    };

                    let config = self
                        .settings
                        .visual_config
                        .clone()
                        .and_then(|cfg| cfg.heatmap());

                    let content = Content::ShaderHeatmap {
                        chart: Some(Box::new(HeatmapShader::new(
                            basis,
                            derived_plan.price_step,
                            base_ticker,
                            studies.clone(),
                            indicators.clone(),
                            config,
                        ))),
                        studies,
                        indicators,
                    };

                    let streams = vec![depth_stream(&derived_plan), trades_stream(&derived_plan)];

                    (content, streams)
                }
                ContentKind::Starter => unreachable!(),
            }
        };

        if let Content::Kline {
            chart: Some(chart),
            indicators,
            ..
        } = &mut content
            && has_trade_history_indicator(indicators)
        {
            let available = data::aggregation::equivalent_footprint_sources(
                base_ticker,
                tickers.iter().copied(),
            );
            let selected = self
                .settings
                .footprint_history_sources
                .as_deref()
                .map(|wanted| {
                    available
                        .iter()
                        .copied()
                        .filter(|source| {
                            wanted
                                .iter()
                                .any(|ticker| ticker.same_market(&source.ticker))
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|sources| !sources.is_empty())
                .unwrap_or(available);
            chart.configure_footprint_history(selected, self.settings.footprint_history_aggregate);
        }

        if let Content::Kline {
            chart: Some(chart),
            indicators,
            ..
        } = &mut content
            && has_liquidity_heatmap(indicators)
        {
            let available = data::aggregation::equivalent_footprint_sources(
                base_ticker,
                tickers.iter().copied(),
            );
            let selected = self
                .settings
                .liquidity_heatmap_sources
                .as_deref()
                .map(|wanted| {
                    available
                        .iter()
                        .copied()
                        .filter(|source| {
                            wanted
                                .iter()
                                .any(|ticker| ticker.same_market(&source.ticker))
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|sources| !sources.is_empty())
                .unwrap_or(available);
            self.settings.liquidity_heatmap_sources =
                Some(selected.iter().map(|source| source.ticker).collect());
            chart.configure_liquidity_heatmap(selected);
        }

        self.content = content;
        self.streams = ResolvedStream::Ready(streams.clone());
        self.sync_footprint_history_streams();
        self.sync_liquidity_heatmap_streams();

        match &self.streams {
            ResolvedStream::Ready(streams) => streams.clone(),
            _ => streams,
        }
    }

    fn sync_footprint_history_streams(&mut self) {
        if let Content::FootprintHistory(Some(history)) = &self.content {
            let sources = history.active_sources().to_vec();
            let ResolvedStream::Ready(streams) = &mut self.streams else {
                return;
            };
            streams.retain(|stream| match stream {
                StreamKind::Trades { ticker_info } => sources
                    .iter()
                    .any(|source| source.ticker.same_market(&ticker_info.ticker)),
                StreamKind::Kline { .. } | StreamKind::Depth { .. } => false,
            });
            for source in sources {
                let wanted = StreamKind::Trades {
                    ticker_info: source,
                };
                if !streams.contains(&wanted) {
                    streams.push(wanted);
                }
            }
            return;
        }

        let Content::Kline {
            chart: Some(chart),
            indicators,
            ..
        } = &self.content
        else {
            return;
        };

        let main_uses_trades = matches!(
            chart.kind(),
            data::chart::KlineChartKind::Footprint { .. }
                | data::chart::KlineChartKind::Renko { .. }
                | data::chart::KlineChartKind::Tpo { .. }
        ) || matches!(chart.basis(), Basis::Tick(_))
            || indicators.contains(&KlineIndicator::VisibleRangeProfile);
        let main_sources = if !main_uses_trades {
            Vec::new()
        } else if matches!(
            chart.kind(),
            data::chart::KlineChartKind::Footprint { .. } | data::chart::KlineChartKind::Tpo { .. }
        ) {
            chart.feed().sources().to_vec()
        } else {
            vec![chart.feed().primary()]
        };
        let indicator_sources = if has_trade_history_indicator(indicators) {
            if chart.footprint_history_aggregate() {
                chart.footprint_history_sources().to_vec()
            } else {
                chart
                    .footprint_history_sources()
                    .get(..1)
                    .unwrap_or(&[])
                    .to_vec()
            }
        } else {
            Vec::new()
        };

        let ResolvedStream::Ready(streams) = &mut self.streams else {
            return;
        };
        streams.retain(|stream| match stream {
            StreamKind::Trades { ticker_info } => main_sources
                .iter()
                .chain(indicator_sources.iter())
                .any(|source| source.ticker.same_market(&ticker_info.ticker)),
            StreamKind::Kline { .. } | StreamKind::Depth { .. } => true,
        });
        for source in main_sources.iter().chain(indicator_sources.iter()) {
            let wanted = StreamKind::Trades {
                ticker_info: *source,
            };
            if !streams.contains(&wanted) {
                streams.push(wanted);
            }
        }
    }

    fn sync_liquidity_heatmap_streams(&mut self) {
        let sources = match &self.content {
            Content::Kline {
                chart: Some(chart),
                indicators,
                ..
            } if has_liquidity_heatmap(indicators) && chart.allows_liquidity_heatmap() => {
                chart.liquidity_heatmap_sources().to_vec()
            }
            Content::Kline { .. } => Vec::new(),
            _ => return,
        };

        let ResolvedStream::Ready(streams) = &mut self.streams else {
            return;
        };
        streams.retain(|stream| !matches!(stream, StreamKind::Depth { .. }));
        for source in sources {
            let wanted = StreamKind::Depth {
                ticker_info: source,
                depth_aggr: source
                    .exchange()
                    .stream_ticksize(Some(TickMultiplier(1)), TickMultiplier(1)),
                push_freq: exchange::PushFrequency::ServerDefault,
            };
            if !streams.contains(&wanted) {
                streams.push(wanted);
            }
        }
    }

    pub fn insert_hist_oi(
        &mut self,
        source: TickerInfo,
        req_id: Option<uuid::Uuid>,
        oi: &[OpenInterest],
    ) {
        match &mut self.content {
            Content::Kline { chart, .. } => {
                let Some(chart) = chart else {
                    panic!("Kline chart wasn't initialized when inserting open interest");
                };
                chart.insert_open_interest(source, req_id, oi);
            }
            Content::FootprintHistory(Some(history)) => {
                history.insert_open_interest(source, oi, req_id);
            }
            _ => {
                log::error!("pane content not candlestick");
            }
        }
    }

    pub fn insert_hist_klines(
        &mut self,
        req_id: Option<uuid::Uuid>,
        timeframe: Timeframe,
        ticker_info: TickerInfo,
        klines: &[Kline],
    ) {
        match &mut self.content {
            Content::Kline {
                chart, indicators, ..
            } => {
                let Some(chart) = chart else {
                    panic!("chart wasn't initialized when inserting klines");
                };

                if let Some(id) = req_id {
                    let accepts_time = chart.basis() == Basis::Time(timeframe);
                    let accepts_tick_seed = chart.accepts_tick_kline_seed(timeframe);
                    // Letter-timeframe bars feeding the Previous Value Areas
                    // overlay are stored separately from the chart's series.
                    // When the letter timeframe equals the chart timeframe the
                    // same page feeds both consumers.
                    let accepts_pva_seed =
                        !accepts_tick_seed && chart.accepts_pva_seed_klines(timeframe);
                    if accepts_pva_seed {
                        chart.insert_pva_seed_klines(id, ticker_info, klines);
                    }
                    if accepts_time || accepts_tick_seed {
                        chart.insert_hist_klines(id, ticker_info, klines);
                    } else if !accepts_pva_seed {
                        log::warn!(
                            "Ignoring stale kline fetch for timeframe {:?}; chart basis = {:?}",
                            timeframe,
                            chart.basis()
                        );
                    }
                } else {
                    let (raw_trades, display_step) = (chart.raw_trades(), chart.tick_size());
                    let live_trade_starts = chart.live_trade_starts();
                    let layout = chart.chart_layout();
                    let visual_config = chart.visual_config();
                    let feed = chart.feed().clone();
                    let footprint_history_sources = chart.footprint_history_sources().to_vec();
                    let footprint_history_aggregate = chart.footprint_history_aggregate();

                    // Rebuild on the feed's base tick so stored footprints
                    // stay multiplier-independent, then restore the display
                    // grouping.
                    let mut rebuilt = KlineChart::new(
                        layout,
                        Basis::Time(timeframe),
                        feed.price_step(),
                        klines,
                        raw_trades,
                        indicators,
                        ticker_info,
                        chart.kind(),
                        Some(visual_config),
                    );
                    rebuilt.set_feed(feed);
                    rebuilt.restore_live_trade_starts(live_trade_starts);
                    rebuilt.change_tick_size(display_step);
                    rebuilt.configure_footprint_history(
                        footprint_history_sources,
                        footprint_history_aggregate,
                    );
                    *chart = rebuilt;
                }
            }
            Content::Comparison(chart) => {
                let Some(chart) = chart else {
                    panic!("Comparison chart wasn't initialized when inserting klines");
                };

                if let Some(id) = req_id {
                    if chart.timeframe != timeframe {
                        log::warn!(
                            "Ignoring stale kline fetch for timeframe {:?}; chart timeframe = {:?}",
                            timeframe,
                            chart.timeframe
                        );
                        return;
                    }
                    chart.insert_history(id, ticker_info, klines);
                } else {
                    *chart = ComparisonChart::new(
                        Basis::Time(timeframe),
                        &[ticker_info],
                        Some(chart.serializable_config()),
                    );
                }
            }
            _ => {
                log::error!("pane content not candlestick or footprint");
            }
        }
    }

    fn has_stream(&self) -> bool {
        match &self.streams {
            ResolvedStream::Ready(streams) => !streams.is_empty(),
            ResolvedStream::Waiting { streams, .. } => !streams.is_empty(),
            ResolvedStream::Blocked { streams, .. } => !streams.is_empty(),
        }
    }

    pub fn view<'a>(
        &'a self,
        id: pane_grid::Pane,
        panes: usize,
        is_focused: bool,
        maximized: bool,
        window: window::Id,
        main_window: &'a Window,
        timezone: UserTimezone,
        tickers_table: &'a TickersTable,
    ) -> pane_grid::Content<'a, Message, Theme, Renderer> {
        let mut top_left_buttons = if Content::Starter == self.content {
            row![]
        } else {
            row![link_group_button(id, self.link_group, |id| {
                Message::PaneEvent(id, Event::ShowModal(Modal::LinkGroup))
            })]
        };

        if let Some(kind) = self.stream_pair_kind() {
            let (base_ti, extra) = match kind {
                StreamPairKind::MultiSource(list) => (list[0], list.len().saturating_sub(1)),
                StreamPairKind::SingleSource(ti) => (ti, 0),
            };

            let exchange_icon = icon_text(style::venue_icon(base_ti.ticker.exchange.venue()), 14);
            let mut label = {
                let symbol = base_ti.ticker.display_symbol_and_type().0;
                match base_ti.ticker.market_type() {
                    MarketKind::Spot => symbol,
                    MarketKind::LinearPerps | MarketKind::InversePerps => symbol + " PERP",
                }
            };
            if extra > 0 {
                label = format!("{label} +{extra}");
            }
            if extra > 0 && self.settings.aggregate_feed.is_some() {
                label = format!("{label} · AGG");
            }

            let content = row![
                exchange_icon.align_y(Alignment::Center).line_height(1.4),
                text(label)
                    .size(crate::style::text_size::SECTION)
                    .align_y(Alignment::Center)
                    .line_height(1.4)
            ]
            .align_y(Alignment::Center)
            .spacing(4);

            let tickers_list_btn = button(content)
                .on_press(Message::PaneEvent(
                    id,
                    Event::ShowModal(Modal::MiniTickersList(MiniPanel::new())),
                ))
                .style(|theme, status| {
                    style::button::modifier(
                        theme,
                        status,
                        !matches!(self.modal, Some(Modal::MiniTickersList(_))),
                    )
                })
                .height(widget::PANE_CONTROL_BTN_HEIGHT);

            top_left_buttons = top_left_buttons.push(tickers_list_btn);
        } else if !matches!(self.content, Content::Starter) && !self.has_stream() {
            let content = row![
                text("Choose a ticker")
                    .size(crate::style::text_size::EMPHASIS)
                    .align_y(Alignment::Center)
                    .line_height(1.4)
            ]
            .align_y(Alignment::Center);

            let tickers_list_btn = button(content)
                .on_press(Message::PaneEvent(
                    id,
                    Event::ShowModal(Modal::MiniTickersList(MiniPanel::new())),
                ))
                .style(|theme, status| {
                    style::button::modifier(
                        theme,
                        status,
                        !matches!(self.modal, Some(Modal::MiniTickersList(_))),
                    )
                })
                .height(widget::PANE_CONTROL_BTN_HEIGHT);

            top_left_buttons = top_left_buttons.push(tickers_list_btn);
        }

        let modifier: Option<modal::stream::Modifier> = self.modal.clone().and_then(|m| {
            if let Modal::StreamModifier(modifier) = m {
                Some(modifier)
            } else {
                None
            }
        });

        let compact_controls = if self.modal == Some(Modal::Controls) {
            Some(
                container(self.view_controls(id, panes, maximized, window != main_window.id))
                    .style(style::chart_modal)
                    .into(),
            )
        } else {
            None
        };

        let uninitialized_base = |kind: ContentKind| -> Element<'a, Message> {
            match &self.streams {
                ResolvedStream::Waiting { streams, .. } if !streams.is_empty() => {
                    center(text("Waiting for metadata…").size(crate::style::text_size::TITLE))
                        .into()
                }
                ResolvedStream::Ready(streams) if !streams.is_empty() => center(
                    text("Waiting for pane initialization...").size(crate::style::text_size::TITLE),
                )
                .into(),
                ResolvedStream::Blocked {
                    streams, reason, ..
                } => {
                    let blocked_exchanges = streams
                        .iter()
                        .map(|s| s.exchange())
                        .collect::<std::collections::BTreeSet<_>>();

                    center(
                        column![
                            text(format!(
                                "Couldn't resolve streams for {}",
                                blocked_exchanges
                                    .iter()
                                    .map(|v| v.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                            .size(crate::style::text_size::SECTION),
                            text((if reason.is_empty() { "" } else { reason }).to_string())
                                .size(crate::style::text_size::BODY),
                        ]
                        .spacing(8)
                        .align_x(Alignment::Center),
                    )
                    .into()
                }
                _ => {
                    let content = column![
                        text(kind.to_string()).size(crate::style::text_size::TITLE),
                        text("No ticker selected").size(crate::style::text_size::SECTION)
                    ]
                    .spacing(8)
                    .align_x(Alignment::Center);

                    center(content).into()
                }
            }
        };

        let body = match &self.content {
            Content::Starter => {
                let content_picklist =
                    pick_list(ContentKind::ALL, Some(ContentKind::Starter), move |kind| {
                        Message::PaneEvent(id, Event::ContentSelected(kind))
                    });

                let base: Element<_> = widget::toast::Manager::new(
                    center(
                        column![
                            text("Choose a view to get started")
                                .size(crate::style::text_size::TITLE),
                            content_picklist
                        ]
                        .align_x(Alignment::Center)
                        .spacing(12),
                    ),
                    &self.notifications,
                    Alignment::End,
                    move |msg| Message::PaneEvent(id, Event::DeleteNotification(msg)),
                )
                .into();

                self.compose_stack_view(
                    base,
                    id,
                    None,
                    compact_controls,
                    || column![].into(),
                    None,
                    tickers_table,
                )
            }
            Content::FootprintHistory(history) => {
                if let Some(history) = history {
                    let ticker_info = history
                        .sources()
                        .iter()
                        .min_by_key(|ticker| ticker.min_ticksize.power)
                        .copied()
                        .or_else(|| self.stream_pair());
                    let tick_multiply = self.settings.tick_multiply.unwrap_or(TickMultiplier(1000));
                    let min_ticksize = ticker_info.map(|ticker| ticker.min_ticksize);
                    let exchange = ticker_info.map(|ticker| ticker.ticker.exchange);
                    let base_step = ticker_info
                        .map(|ticker| {
                            tick_multiply
                                .unscale_step_or_min_tick(history.block_step(), ticker.min_ticksize)
                        })
                        .unwrap_or_else(|| tick_multiply.unscale_step(history.block_step()));
                    top_left_buttons = top_left_buttons.push(footprint_history_modifier(
                        id,
                        history.block_step(),
                        base_step,
                        min_ticksize,
                        tick_multiply,
                        modifier,
                        exchange,
                    ));

                    let available = ticker_info.map_or_else(Vec::new, |selected| {
                        data::aggregation::equivalent_footprint_sources(
                            selected,
                            tickers_table.tickers_info.values().filter_map(|info| *info),
                        )
                    });
                    let source_choices = available
                        .into_iter()
                        .map(|source| {
                            let selected = history
                                .sources()
                                .iter()
                                .any(|active| active.ticker.same_market(&source.ticker));
                            let symbol = source.ticker.display_symbol_and_type().0;
                            (
                                source,
                                format!("{} · {}", source.exchange().venue(), symbol),
                                selected,
                            )
                        })
                        .collect::<Vec<_>>();
                    let aggregate = history.aggregate();
                    let settings_modal =
                        move || footprint_history_cfg_view(id, source_choices.clone(), aggregate);
                    let base = history.view().map(move |message| {
                        Message::PaneEvent(id, Event::ChartInteraction(message))
                    });

                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::FootprintHistory);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::Comparison(chart) => {
                if let Some(c) = chart {
                    let selected_basis = Basis::Time(c.timeframe);
                    let kind = ModifierKind::Comparison(selected_basis);

                    let modifiers =
                        row![basis_modifier(id, selected_basis, modifier, kind),].spacing(4);

                    top_left_buttons = top_left_buttons.push(modifiers);

                    let base = c.view(timezone).map(move |message| {
                        Message::PaneEvent(id, Event::ComparisonChartInteraction(message))
                    });

                    let settings_modal = || comparison_cfg_view(id, c);

                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        settings_modal,
                        Some(c.selected_tickers()),
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::ComparisonChart);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::TimeAndSales(panel) => {
                if let Some(panel) = panel {
                    let base = panel::view(panel, timezone).map(move |message| {
                        Message::PaneEvent(id, Event::PanelInteraction(message))
                    });

                    let settings_modal =
                        || modal::pane::settings::timesales_cfg_view(panel.config, id);

                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::TimeAndSales);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::Ladder(panel) => {
                if let Some(panel) = panel {
                    let basis = self
                        .settings
                        .selected_basis
                        .unwrap_or(Basis::default_heatmap_time(self.stream_pair()));
                    let tick_multiply = self.settings.tick_multiply.unwrap_or(TickMultiplier(1));

                    let stream_pair = self.stream_pair();

                    let price_step = stream_pair
                        .map(|ti| {
                            tick_multiply.unscale_step_or_min_tick(panel.step, ti.min_ticksize)
                        })
                        .unwrap_or_else(|| tick_multiply.unscale_step(panel.step));

                    let exchange = stream_pair.map(|ti| ti.ticker.exchange);
                    let min_ticksize = stream_pair.map(|ti| ti.min_ticksize);

                    let modifiers = ticksize_modifier(
                        id,
                        price_step,
                        min_ticksize,
                        tick_multiply,
                        modifier,
                        ModifierKind::Orderbook(basis, tick_multiply),
                        exchange,
                    );

                    top_left_buttons = top_left_buttons.push(modifiers);

                    let base = panel::view(panel, timezone).map(move |message| {
                        Message::PaneEvent(id, Event::PanelInteraction(message))
                    });

                    let settings_modal =
                        || modal::pane::settings::ladder_cfg_view(panel.config, id);

                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::Ladder);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::Heatmap {
                chart, indicators, ..
            } => {
                if let Some(chart) = chart {
                    let ticker_info = self.stream_pair();
                    let exchange = ticker_info.as_ref().map(|info| info.ticker.exchange);

                    let basis = self
                        .settings
                        .selected_basis
                        .unwrap_or(Basis::default_heatmap_time(ticker_info));
                    let tick_multiply = self.settings.tick_multiply.unwrap_or(TickMultiplier(5));

                    let kind = ModifierKind::Heatmap(basis, tick_multiply);
                    let price_step = ticker_info
                        .map(|ti| {
                            tick_multiply
                                .unscale_step_or_min_tick(chart.tick_size(), ti.min_ticksize)
                        })
                        .unwrap_or_else(|| tick_multiply.unscale_step(chart.tick_size()));
                    let min_ticksize = ticker_info.map(|ti| ti.min_ticksize);

                    let modifiers = row![
                        basis_modifier(id, basis, modifier, kind),
                        ticksize_modifier(
                            id,
                            price_step,
                            min_ticksize,
                            tick_multiply,
                            modifier,
                            kind,
                            exchange
                        ),
                    ]
                    .spacing(4);

                    top_left_buttons = top_left_buttons.push(modifiers);

                    let base = chart::view(chart, indicators, timezone).map(move |message| {
                        Message::PaneEvent(id, Event::ChartInteraction(message))
                    });
                    let settings_modal = || {
                        heatmap_cfg_view(
                            chart.visual_config(),
                            id,
                            chart.study_configurator(),
                            &chart.studies,
                            basis,
                        )
                    };

                    let indicator_modal = if self.modal == Some(Modal::Indicators) {
                        Some(modal::indicators::view(
                            id,
                            self,
                            indicators,
                            self.stream_pair().map(|i| i.ticker.market_type()),
                            &[],
                        ))
                    } else {
                        None
                    };

                    self.compose_stack_view(
                        base,
                        id,
                        indicator_modal,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::HeatmapChart);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::Kline {
                chart,
                indicators,
                kind: chart_kind,
                ..
            } => {
                if let Some(chart) = chart {
                    match chart_kind {
                        data::chart::KlineChartKind::Footprint { .. } => {
                            let basis = chart.basis();
                            let tick_multiply =
                                self.settings.tick_multiply.unwrap_or(TickMultiplier(10));

                            let kind = ModifierKind::Footprint(basis, tick_multiply);
                            let stream_pair = self.stream_pair();
                            let price_step = stream_pair
                                .map(|ti| {
                                    tick_multiply.unscale_step_or_min_tick(
                                        chart.tick_size(),
                                        ti.min_ticksize,
                                    )
                                })
                                .unwrap_or_else(|| tick_multiply.unscale_step(chart.tick_size()));

                            let exchange = stream_pair.as_ref().map(|info| info.ticker.exchange);
                            let min_ticksize = stream_pair.map(|ti| ti.min_ticksize);

                            let modifiers = row![
                                basis_modifier(id, basis, modifier, kind),
                                ticksize_modifier(
                                    id,
                                    price_step,
                                    min_ticksize,
                                    tick_multiply,
                                    modifier,
                                    kind,
                                    exchange
                                ),
                            ]
                            .spacing(4);

                            top_left_buttons = top_left_buttons.push(modifiers);
                        }
                        data::chart::KlineChartKind::Renko { config } => {
                            top_left_buttons = top_left_buttons.push(renko_modifier(
                                id,
                                *config,
                                self.modal == Some(Modal::Settings),
                            ));
                        }
                        data::chart::KlineChartKind::Tpo { config } => {
                            top_left_buttons = top_left_buttons.push(tpo_modifier(
                                id,
                                *config,
                                self.modal == Some(Modal::Settings),
                            ));
                        }
                        data::chart::KlineChartKind::Candles => {
                            let selected_basis = chart.basis();
                            let kind = ModifierKind::Candlestick(selected_basis);

                            let modifiers =
                                row![basis_modifier(id, selected_basis, modifier, kind),]
                                    .spacing(4);

                            top_left_buttons = top_left_buttons.push(modifiers);
                        }
                    }

                    let base = chart::view(chart, indicators, timezone).map(move |message| {
                        Message::PaneEvent(id, Event::ChartInteraction(message))
                    });
                    let footprint_history_available =
                        data::aggregation::equivalent_footprint_sources(
                            chart.feed().primary(),
                            tickers_table.tickers_info.values().filter_map(|info| *info),
                        );
                    let settings_modal = || {
                        let catalog_sources = chart.feed().id().map_or_else(Vec::new, |feed| {
                            feed.definition()
                                .sources
                                .iter()
                                .filter_map(|definition| {
                                    tickers_table.tickers_info.iter().find_map(
                                        |(ticker, ticker_info)| {
                                            definition
                                                .matches(*ticker)
                                                .then_some(*ticker_info)
                                                .flatten()
                                        },
                                    )
                                })
                                .map(|ticker_info| {
                                    let selected = chart.feed().sources().iter().any(|source| {
                                        source.ticker.same_market(&ticker_info.ticker)
                                    });
                                    let label = feed
                                        .source_label(ticker_info.ticker)
                                        .unwrap_or("Unknown venue")
                                        .to_string();
                                    (ticker_info, label, selected)
                                })
                                .collect::<Vec<_>>()
                        });
                        let aggregate_sources = if !catalog_sources.is_empty() {
                            catalog_sources
                        } else {
                            footprint_history_available
                                .iter()
                                .copied()
                                .map(|ticker_info| {
                                    let selected = chart.feed().sources().iter().any(|source| {
                                        source.ticker.same_market(&ticker_info.ticker)
                                    });
                                    let symbol = ticker_info.ticker.display_symbol_and_type().0;
                                    let label =
                                        format!("{} · {}", ticker_info.exchange().venue(), symbol);
                                    (ticker_info, label, selected)
                                })
                                .collect()
                        };
                        let footprint_history =
                            has_trade_history_indicator(indicators).then(|| {
                                let sources = footprint_history_available
                                    .iter()
                                    .copied()
                                    .map(|source| {
                                        let selected =
                                            chart.footprint_history_sources().iter().any(
                                                |active| active.ticker.same_market(&source.ticker),
                                            );
                                        let symbol = source.ticker.display_symbol_and_type().0;
                                        (
                                            source,
                                            format!("{} · {}", source.exchange().venue(), symbol),
                                            selected,
                                        )
                                    })
                                    .collect();
                                (sources, chart.footprint_history_aggregate())
                            });
                        let liquidity_heatmap = indicators
                            .contains(&KlineIndicator::LiquidityHeatmap)
                            .then(|| {
                                footprint_history_available
                                    .iter()
                                    .copied()
                                    .map(|source| {
                                        let selected =
                                            chart.liquidity_heatmap_sources().iter().any(
                                                |active| active.ticker.same_market(&source.ticker),
                                            );
                                        let symbol = source.ticker.display_symbol_and_type().0;
                                        (
                                            source,
                                            format!("{} · {}", source.exchange().venue(), symbol),
                                            selected,
                                        )
                                    })
                                    .collect()
                            });
                        kline_cfg_view(
                            chart.study_configurator(),
                            chart.visual_config(),
                            chart_kind,
                            id,
                            chart.basis(),
                            chart.tick_size(),
                            self.settings.tick_multiply.unwrap_or(TickMultiplier(50)),
                            aggregate_sources,
                            footprint_history,
                            liquidity_heatmap,
                            indicators.contains(&KlineIndicator::DailyDelta).then(|| {
                                let cfg = chart.visual_config();
                                (cfg.daily_delta_ticks, cfg.daily_delta_days)
                            }),
                            indicators
                                .contains(&KlineIndicator::PreviousValueArea)
                                .then(|| chart.visual_config().previous_value_area_ticks),
                            indicators
                                .contains(&KlineIndicator::LargeTrades)
                                .then(|| chart.visual_config().large_trades_min_usd),
                            indicators
                                .contains(&KlineIndicator::VisibleRangeProfile)
                                .then(|| chart.visual_config().vpvr_ticks),
                        )
                    };

                    let indicator_modal = if self.modal == Some(Modal::Indicators) {
                        Some(modal::indicators::view(
                            id,
                            self,
                            indicators,
                            self.stream_pair().map(|i| i.ticker.market_type()),
                            &footprint_history_available,
                        ))
                    } else {
                        None
                    };

                    self.compose_stack_view(
                        base,
                        id,
                        indicator_modal,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let content_kind = match chart_kind {
                        data::chart::KlineChartKind::Candles => ContentKind::CandlestickChart,
                        data::chart::KlineChartKind::Renko { .. } => ContentKind::RenkoChart,
                        data::chart::KlineChartKind::Tpo { .. } => ContentKind::TpoChart,
                        data::chart::KlineChartKind::Footprint { .. } => {
                            ContentKind::FootprintChart
                        }
                    };
                    let base = uninitialized_base(content_kind);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
            Content::ShaderHeatmap {
                chart, indicators, ..
            } => {
                if let Some(chart) = chart {
                    let base = HeatmapShader::view(chart, timezone).map(move |message| {
                        Message::PaneEvent(id, Event::HeatmapShaderInteraction(message))
                    });

                    let ticker_info = self.stream_pair();
                    let exchange = ticker_info.as_ref().map(|info| info.ticker.exchange);

                    let basis = self
                        .settings
                        .selected_basis
                        .unwrap_or(Basis::default_heatmap_time(ticker_info));
                    let tick_multiply = self.settings.tick_multiply.unwrap_or(TickMultiplier(5));

                    let kind = ModifierKind::Heatmap(basis, tick_multiply);

                    let price_step = ticker_info
                        .map(|ti| {
                            tick_multiply
                                .unscale_step_or_min_tick(chart.tick_size(), ti.min_ticksize)
                        })
                        .unwrap_or_else(|| tick_multiply.unscale_step(chart.tick_size()));
                    let min_ticksize = ticker_info.map(|ti| ti.min_ticksize);

                    let settings_modal = || {
                        heatmap_shader_cfg_view(
                            chart.visual_config(),
                            id,
                            chart.study_configurator(),
                            &chart.studies,
                            basis,
                        )
                    };

                    let indicator_modal = if self.modal == Some(Modal::Indicators) {
                        Some(modal::indicators::view(
                            id,
                            self,
                            indicators,
                            self.stream_pair().map(|i| i.ticker.market_type()),
                            &[],
                        ))
                    } else {
                        None
                    };

                    let modifiers = row![
                        basis_modifier(id, basis, modifier, kind),
                        ticksize_modifier(
                            id,
                            price_step,
                            min_ticksize,
                            tick_multiply,
                            modifier,
                            kind,
                            exchange
                        ),
                    ]
                    .spacing(4);

                    top_left_buttons = top_left_buttons.push(modifiers);

                    self.compose_stack_view(
                        base,
                        id,
                        indicator_modal,
                        compact_controls,
                        settings_modal,
                        None,
                        tickers_table,
                    )
                } else {
                    let base = uninitialized_base(ContentKind::HeatmapChart);
                    self.compose_stack_view(
                        base,
                        id,
                        None,
                        compact_controls,
                        || column![].into(),
                        None,
                        tickers_table,
                    )
                }
            }
        };

        match &self.status {
            Status::Loading(InfoKind::FetchingKlines) => {
                top_left_buttons = top_left_buttons.push(text("Fetching Klines..."));
            }
            Status::Loading(InfoKind::FetchingTrades(count)) => {
                top_left_buttons =
                    top_left_buttons.push(text(format!("Fetching Trades... {count} fetched")));
            }
            Status::Loading(InfoKind::FetchingOI) => {
                top_left_buttons = top_left_buttons.push(text("Fetching Open Interest..."));
            }
            Status::Stale(msg) => {
                top_left_buttons = top_left_buttons.push(text(msg));
            }
            Status::Ready => {}
        }

        let content = pane_grid::Content::new(body)
            .style(move |theme| style::pane_background(theme, is_focused));

        let top_right_buttons = {
            let compact_control = container(
                button(
                    text("...")
                        .size(crate::style::text_size::EMPHASIS)
                        .align_y(Alignment::End),
                )
                .on_press(Message::PaneEvent(id, Event::ShowModal(Modal::Controls)))
                .style(move |theme, status| {
                    style::button::transparent(
                        theme,
                        status,
                        self.modal == Some(Modal::Controls) || self.modal == Some(Modal::Settings),
                    )
                }),
            )
            .align_y(Alignment::Center)
            .padding(4);

            if self.modal == Some(Modal::Controls) {
                pane_grid::Controls::new(compact_control)
            } else {
                pane_grid::Controls::dynamic(
                    self.view_controls(id, panes, maximized, window != main_window.id),
                    compact_control,
                )
            }
        };

        let title_bar = pane_grid::TitleBar::new(
            top_left_buttons
                .padding(padding::left(4))
                .align_y(Alignment::Center)
                .spacing(8)
                .height(Length::Fixed(32.0)),
        )
        .controls(top_right_buttons)
        .style(style::pane_title_bar);

        content.title_bar(if self.modal.is_none() {
            title_bar
        } else {
            title_bar.always_show_controls()
        })
    }

    fn apply_tick_multiplier(&mut self, tm: TickMultiplier) -> Option<Effect> {
        self.settings.tick_multiply = Some(tm);
        let ticker = self.stream_pair();

        match &mut self.content {
            Content::Kline { chart: Some(c), .. } => {
                let is_footprint =
                    matches!(c.kind(), data::chart::KlineChartKind::Footprint { .. });
                let step = if is_footprint {
                    tm.multiply_step(c.feed().price_step())
                } else {
                    tm.multiply_with_min_tick_step(ticker?)
                };
                c.change_tick_size(step);
                if is_footprint {
                    // Footprints regroup onto the new step at draw time from
                    // base-tick storage, so no stream refresh is needed.
                    return None;
                }
                c.reset_request_handler();
            }
            Content::Heatmap { chart: Some(c), .. } => {
                c.change_tick_size(tm.multiply_with_min_tick_step(ticker?));
            }
            Content::Ladder(Some(p)) => {
                p.set_tick_size(tm.multiply_with_min_tick_step(ticker?));
            }
            Content::FootprintHistory(Some(history)) => {
                let ticker = ticker?;
                let min_step = history
                    .sources()
                    .iter()
                    .map(|source| PriceStep::from(source.min_ticksize))
                    .min_by_key(|step| step.units)
                    .unwrap_or_else(|| PriceStep::from(ticker.min_ticksize));
                history.set_block_step(tm.multiply_step(min_step));
            }
            Content::ShaderHeatmap {
                chart: Some(c),
                indicators,
                studies,
                ..
            } => {
                let saved_config = c.config;
                **c = HeatmapShader::new(
                    c.basis,
                    tm.multiply_with_min_tick_step(ticker?),
                    c.ticker_info,
                    studies.clone(),
                    indicators.clone(),
                    Some(saved_config),
                );
            }
            _ => {}
        }

        let is_client = ticker
            .map(|ti| ti.exchange().is_depth_client_aggr())
            .unwrap_or(false);

        if let Some(mut it) = self.streams.ready_iter_mut() {
            for s in &mut it {
                if let StreamKind::Depth { depth_aggr, .. } = s {
                    *depth_aggr = if is_client {
                        StreamTicksize::Client
                    } else {
                        StreamTicksize::ServerSide(tm)
                    };
                }
            }
        }
        (!is_client).then_some(Effect::RefreshStreams)
    }

    pub fn update(&mut self, msg: Event) -> Option<Effect> {
        match msg {
            Event::ShowModal(requested_modal) => {
                return self.show_modal_with_focus(requested_modal);
            }
            Event::HideModal => {
                self.modal = None;
            }
            Event::ContentSelected(kind) => {
                self.content = Content::placeholder(kind);
                self.settings.visual_config = None;

                if !matches!(kind, ContentKind::Starter) {
                    self.streams = ResolvedStream::waiting(vec![]);
                    let modal = Modal::MiniTickersList(MiniPanel::new());

                    if let Some(effect) = self.show_modal_with_focus(modal) {
                        return Some(effect);
                    }
                }
            }
            Event::ChartInteraction(msg) => match &mut self.content {
                Content::Heatmap { chart: Some(c), .. } => {
                    super::chart::update(c, &msg);
                }
                Content::Kline { chart: Some(c), .. } => {
                    super::chart::update(c, &msg);
                }
                _ => {}
            },
            Event::PanelInteraction(msg) => match &mut self.content {
                Content::Ladder(Some(p)) => super::panel::update(p, msg),
                Content::TimeAndSales(Some(p)) => super::panel::update(p, msg),
                _ => {}
            },
            Event::ToggleIndicator(ind, available_sources) => {
                let is_trade_history = matches!(
                    ind,
                    UiIndicator::Kline(indicator) if indicator.needs_trade_history()
                );
                let is_visible_trades = matches!(
                    ind,
                    UiIndicator::Kline(indicator) if indicator.needs_visible_trades()
                );
                let enabling = is_trade_history
                    && matches!(
                        &self.content,
                        Content::Kline { indicators, .. }
                            if !has_trade_history_indicator(indicators)
                    );
                let is_liquidity_heatmap =
                    matches!(ind, UiIndicator::Kline(KlineIndicator::LiquidityHeatmap));
                let is_open_interest =
                    matches!(ind, UiIndicator::Kline(KlineIndicator::OpenInterest));
                let enabling_open_interest = is_open_interest
                    && matches!(
                        &self.content,
                        Content::Kline { indicators, .. }
                            if !indicators.contains(&KlineIndicator::OpenInterest)
                    );
                let enabling_liquidity = is_liquidity_heatmap
                    && matches!(
                        &self.content,
                        Content::Kline { indicators, .. }
                            if !has_liquidity_heatmap(indicators)
                    );

                if enabling {
                    let configured = self
                        .settings
                        .footprint_history_sources
                        .as_deref()
                        .map(|selected| {
                            available_sources
                                .iter()
                                .copied()
                                .filter(|source| {
                                    selected
                                        .iter()
                                        .any(|ticker| ticker.same_market(&source.ticker))
                                })
                                .collect::<Vec<_>>()
                        })
                        .filter(|sources| !sources.is_empty())
                        .unwrap_or_else(|| default_footprint_history_sources(&available_sources));
                    let aggregate = self.settings.footprint_history_sources.is_none()
                        || self.settings.footprint_history_aggregate;
                    self.settings.footprint_history_aggregate = aggregate;
                    self.settings.footprint_history_sources =
                        Some(configured.iter().map(|source| source.ticker).collect());
                    if let Content::Kline {
                        chart: Some(chart), ..
                    } = &mut self.content
                    {
                        chart.configure_footprint_history(configured, aggregate);
                    }
                }

                if enabling_liquidity {
                    let configured = self
                        .settings
                        .liquidity_heatmap_sources
                        .as_deref()
                        .map(|selected| {
                            available_sources
                                .iter()
                                .copied()
                                .filter(|source| {
                                    selected
                                        .iter()
                                        .any(|ticker| ticker.same_market(&source.ticker))
                                })
                                .collect::<Vec<_>>()
                        })
                        .filter(|sources| !sources.is_empty())
                        .unwrap_or_else(|| available_sources.clone());
                    self.settings.liquidity_heatmap_sources =
                        Some(configured.iter().map(|source| source.ticker).collect());
                    if let Content::Kline {
                        chart: Some(chart), ..
                    } = &mut self.content
                    {
                        chart.configure_liquidity_heatmap(configured);
                    }
                }

                if enabling_open_interest
                    && let Content::Kline {
                        chart: Some(chart), ..
                    } = &mut self.content
                {
                    chart.configure_open_interest(available_sources.clone());
                }

                self.content.toggle_indicator(ind);
                if is_trade_history || is_visible_trades {
                    self.sync_footprint_history_streams();
                    return Some(Effect::RefreshStreams);
                }
                if is_liquidity_heatmap {
                    if let Content::Kline {
                        chart: Some(chart),
                        indicators,
                        ..
                    } = &mut self.content
                        && has_liquidity_heatmap(indicators)
                    {
                        let sources = self
                            .settings
                            .liquidity_heatmap_sources
                            .as_deref()
                            .map(|wanted| {
                                available_sources
                                    .iter()
                                    .copied()
                                    .filter(|source| {
                                        wanted
                                            .iter()
                                            .any(|ticker| ticker.same_market(&source.ticker))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .filter(|sources| !sources.is_empty())
                            .unwrap_or_else(|| available_sources.clone());
                        chart.configure_liquidity_heatmap(sources);
                    }
                    self.sync_liquidity_heatmap_streams();
                    return Some(Effect::RefreshStreams);
                }
            }
            Event::DeleteNotification(idx) => {
                if idx < self.notifications.len() {
                    self.notifications.remove(idx);
                }
            }
            Event::ReorderIndicator(e) => {
                self.content.reorder_indicators(&e);
            }
            Event::ClusterKindSelected(kind) => {
                if let Content::Kline {
                    chart, kind: cur, ..
                } = &mut self.content
                    && let Some(c) = chart
                {
                    c.set_cluster_kind(kind);
                    *cur = c.kind.clone();
                }
            }
            Event::ClusterScalingSelected(scaling) => {
                if let Content::Kline { chart, kind, .. } = &mut self.content
                    && let Some(c) = chart
                {
                    c.set_cluster_scaling(scaling);
                    *kind = c.kind.clone();
                }
            }
            Event::TicksizeSelected(tm) => {
                return self.apply_tick_multiplier(tm);
            }
            Event::RenkoConfigChanged(config) => {
                if let Content::Kline { chart, kind, .. } = &mut self.content
                    && let Some(c) = chart
                {
                    c.set_renko_config(config);
                    *kind = c.kind.clone();
                }
            }
            Event::TpoConfigChanged(config) => {
                if let Content::Kline { chart, kind, .. } = &mut self.content
                    && let Some(c) = chart
                {
                    c.set_tpo_config(config);
                    *kind = c.kind.clone();
                }
            }
            Event::AggregateSourceToggled(ticker_info, enabled) => {
                let Content::Kline {
                    chart: Some(chart), ..
                } = &self.content
                else {
                    return None;
                };
                if chart.feed().available_sources().len() <= 1 && chart.feed().id().is_none() {
                    return None;
                }
                let content_kind = match chart.kind() {
                    data::chart::KlineChartKind::Footprint { .. } => ContentKind::FootprintChart,
                    data::chart::KlineChartKind::Tpo { .. } => ContentKind::TpoChart,
                    _ => return None,
                };
                let mut available_sources = chart.feed().available_sources().to_vec();
                let mut selected_sources = chart
                    .feed()
                    .sources()
                    .iter()
                    .map(|source| source.ticker)
                    .collect::<Vec<_>>();

                if enabled {
                    if !available_sources
                        .iter()
                        .any(|source| source.ticker.same_market(&ticker_info.ticker))
                    {
                        available_sources.push(ticker_info);
                    }
                    if !selected_sources
                        .iter()
                        .any(|ticker| ticker.same_market(&ticker_info.ticker))
                    {
                        selected_sources.push(ticker_info.ticker);
                    }
                } else {
                    selected_sources.retain(|ticker| !ticker.same_market(&ticker_info.ticker));
                    if selected_sources.is_empty() {
                        self.notifications.push(Toast::warn(
                            "At least one data source must remain enabled".to_string(),
                        ));
                        return None;
                    }
                }

                self.settings.aggregate_sources = Some(selected_sources);
                self.set_content_and_streams(available_sources, content_kind);
                return Some(Effect::RefreshStreams);
            }
            Event::FootprintHistoryAggregationToggled(aggregate) => {
                let (mut sources, current) = (match &self.content {
                    Content::Kline {
                        chart: Some(chart), ..
                    } => Some((
                        chart.footprint_history_sources().to_vec(),
                        chart.footprint_history_aggregate(),
                    )),
                    Content::FootprintHistory(Some(history)) => {
                        Some((history.sources().to_vec(), history.aggregate()))
                    }
                    _ => None,
                })?;
                if aggregate == current {
                    return None;
                }
                if !aggregate {
                    sources.truncate(1);
                }
                self.settings.footprint_history_aggregate = aggregate;
                self.settings.footprint_history_sources =
                    Some(sources.iter().map(|source| source.ticker).collect());
                match &mut self.content {
                    Content::Kline {
                        chart: Some(chart), ..
                    } => chart.configure_footprint_history(sources, aggregate),
                    Content::FootprintHistory(Some(history)) => {
                        history.configure_sources(sources, aggregate);
                    }
                    _ => {}
                }
                self.sync_footprint_history_streams();
                return Some(Effect::RefreshStreams);
            }
            Event::FootprintHistorySourceToggled(ticker_info, enabled) => {
                let (mut sources, aggregate) = (match &self.content {
                    Content::Kline {
                        chart: Some(chart), ..
                    } => Some((
                        chart.footprint_history_sources().to_vec(),
                        chart.footprint_history_aggregate(),
                    )),
                    Content::FootprintHistory(Some(history)) => {
                        Some((history.sources().to_vec(), history.aggregate()))
                    }
                    _ => None,
                })?;

                if enabled {
                    if aggregate {
                        if !sources
                            .iter()
                            .any(|source| source.ticker.same_market(&ticker_info.ticker))
                        {
                            sources.push(ticker_info);
                        }
                    } else {
                        sources = vec![ticker_info];
                    }
                } else {
                    sources.retain(|source| !source.ticker.same_market(&ticker_info.ticker));
                    if sources.is_empty() {
                        self.notifications.push(Toast::warn(
                            "At least one Footprint History venue must remain enabled".to_string(),
                        ));
                        return None;
                    }
                }

                self.settings.footprint_history_sources =
                    Some(sources.iter().map(|source| source.ticker).collect());
                match &mut self.content {
                    Content::Kline {
                        chart: Some(chart), ..
                    } => chart.configure_footprint_history(sources, aggregate),
                    Content::FootprintHistory(Some(history)) => {
                        history.configure_sources(sources, aggregate);
                    }
                    _ => {}
                }
                self.sync_footprint_history_streams();
                return Some(Effect::RefreshStreams);
            }
            Event::LiquidityHeatmapSourceToggled(ticker_info, enabled) => {
                let mut sources = (match &self.content {
                    Content::Kline {
                        chart: Some(chart), ..
                    } => Some(chart.liquidity_heatmap_sources().to_vec()),
                    _ => None,
                })?;

                if enabled {
                    if !sources
                        .iter()
                        .any(|source| source.ticker.same_market(&ticker_info.ticker))
                    {
                        sources.push(ticker_info);
                    }
                } else {
                    sources.retain(|source| !source.ticker.same_market(&ticker_info.ticker));
                    if sources.is_empty() {
                        self.notifications.push(Toast::warn(
                            "At least one liquidity venue must remain enabled".to_string(),
                        ));
                        return None;
                    }
                }

                self.settings.liquidity_heatmap_sources =
                    Some(sources.iter().map(|source| source.ticker).collect());
                if let Content::Kline {
                    chart: Some(chart), ..
                } = &mut self.content
                {
                    chart.configure_liquidity_heatmap(sources);
                }
                self.sync_liquidity_heatmap_streams();
                return Some(Effect::RefreshStreams);
            }
            Event::StudyConfigurator(study_msg) => match study_msg {
                modal::pane::settings::study::StudyMessage::Footprint(m) => {
                    if let Content::Kline { chart, kind, .. } = &mut self.content
                        && let Some(c) = chart
                    {
                        c.update_study_configurator(m);
                        *kind = c.kind.clone();
                    }
                }
                modal::pane::settings::study::StudyMessage::Heatmap(m) => {
                    if let Content::Heatmap { chart, studies, .. } = &mut self.content
                        && let Some(c) = chart
                    {
                        c.update_study_configurator(m);
                        *studies = c.studies.clone();
                    } else if let Content::ShaderHeatmap { chart, studies, .. } = &mut self.content
                        && let Some(c) = chart
                    {
                        c.update_study_configurator(m);
                        *studies = c.studies.clone();
                    }
                }
            },
            Event::StreamModifierChanged(message) => {
                if let Some(Modal::StreamModifier(mut modifier)) = self.modal.take() {
                    let mut effect: Option<Effect> = None;

                    if let Some(action) = modifier.update(message) {
                        match action {
                            modal::stream::Action::TabSelected(tab) => {
                                modifier.tab = tab;
                            }
                            modal::stream::Action::TicksizeSelected(tm) => {
                                modifier.update_kind_with_multiplier(tm);
                                effect = self.apply_tick_multiplier(tm);
                            }
                            modal::stream::Action::BasisSelected(new_basis) => {
                                modifier.update_kind_with_basis(new_basis);
                                self.settings.selected_basis = Some(new_basis);

                                let base_ticker = self.stream_pair();

                                match &mut self.content {
                                    Content::Heatmap { chart: Some(c), .. } => {
                                        c.set_basis(new_basis);

                                        if let Some(stream_type) =
                                            self.streams.ready_iter_mut().and_then(|mut it| {
                                                it.find(|s| matches!(s, StreamKind::Depth { .. }))
                                            })
                                            && let StreamKind::Depth {
                                                push_freq,
                                                ticker_info,
                                                ..
                                            } = stream_type
                                            && ticker_info.exchange().is_custom_push_freq()
                                        {
                                            match new_basis {
                                                Basis::Time(tf) => {
                                                    *push_freq = exchange::PushFrequency::Custom(tf)
                                                }
                                                Basis::Tick(_) => {
                                                    *push_freq =
                                                        exchange::PushFrequency::ServerDefault
                                                }
                                            }
                                        }

                                        effect = Some(Effect::RefreshStreams);
                                    }
                                    Content::ShaderHeatmap {
                                        chart: Some(c),
                                        indicators,
                                        ..
                                    } => {
                                        let saved_config = c.config;
                                        let saved_studies = c.studies.clone();
                                        **c = HeatmapShader::new(
                                            new_basis,
                                            c.tick_size(),
                                            c.ticker_info,
                                            saved_studies,
                                            indicators.clone(),
                                            Some(saved_config),
                                        );

                                        if let Some(stream_type) =
                                            self.streams.ready_iter_mut().and_then(|mut it| {
                                                it.find(|s| matches!(s, StreamKind::Depth { .. }))
                                            })
                                            && let StreamKind::Depth {
                                                push_freq,
                                                ticker_info,
                                                ..
                                            } = stream_type
                                            && ticker_info.exchange().is_custom_push_freq()
                                        {
                                            match new_basis {
                                                Basis::Time(tf) => {
                                                    *push_freq = exchange::PushFrequency::Custom(tf)
                                                }
                                                Basis::Tick(_) => {
                                                    *push_freq =
                                                        exchange::PushFrequency::ServerDefault
                                                }
                                            }
                                        }

                                        effect = Some(Effect::RefreshStreams);
                                    }
                                    Content::Kline { chart: Some(c), .. } => {
                                        if let Some(base_ticker) = base_ticker {
                                            match new_basis {
                                                Basis::Time(tf) => {
                                                    let kline_stream = StreamKind::Kline {
                                                        ticker_info: base_ticker,
                                                        timeframe: tf,
                                                    };
                                                    let mut streams = vec![kline_stream];

                                                    if matches!(
                                                        c.kind,
                                                        data::chart::KlineChartKind::Footprint { .. }
                                                    ) {
                                                        streams.push(StreamKind::Trades {
                                                            ticker_info: base_ticker,
                                                        });
                                                    }

                                                    self.streams = ResolvedStream::Ready(streams);
                                                    let action = c.set_basis(new_basis);

                                                    if let Some(chart::Action::RequestFetch(
                                                        fetch,
                                                    )) = action
                                                    {
                                                        effect = Some(Effect::RequestFetch(fetch));
                                                    }
                                                }
                                                Basis::Tick(_) => {
                                                    self.streams = ResolvedStream::Ready(vec![
                                                        StreamKind::Trades {
                                                            ticker_info: base_ticker,
                                                        },
                                                    ]);
                                                    c.set_basis(new_basis);

                                                    self.status = Status::Ready;
                                                    effect = Some(Effect::RefreshStreams);
                                                }
                                            }
                                        }
                                    }
                                    Content::Comparison(Some(c)) => {
                                        if let Basis::Time(tf) = new_basis {
                                            let streams: Vec<StreamKind> = c
                                                .selected_tickers()
                                                .iter()
                                                .copied()
                                                .map(|ti| StreamKind::Kline {
                                                    ticker_info: ti,
                                                    timeframe: tf,
                                                })
                                                .collect();

                                            self.streams = ResolvedStream::Ready(streams);
                                            let action = c.set_basis(new_basis);

                                            if let Some(chart::Action::RequestFetch(fetch)) = action
                                            {
                                                effect = Some(Effect::RequestFetch(fetch));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        self.sync_liquidity_heatmap_streams();
                    }

                    self.modal = Some(Modal::StreamModifier(modifier));

                    if let Some(e) = effect {
                        return Some(e);
                    }
                }
            }
            Event::ComparisonChartInteraction(message) => {
                if let Content::Comparison(chart_opt) = &mut self.content
                    && let Some(chart) = chart_opt
                    && let Some(action) = chart.update(message)
                {
                    match action {
                        super::chart::comparison::Action::SeriesColorChanged(t, color) => {
                            chart.set_series_color(t, color);
                        }
                        super::chart::comparison::Action::SeriesNameChanged(t, name) => {
                            chart.set_series_name(t, name);
                        }
                        super::chart::comparison::Action::OpenSeriesEditor => {
                            self.modal = Some(Modal::Settings);
                        }
                        super::chart::comparison::Action::RemoveSeries(ti) => {
                            let rebuilt = chart.remove_ticker(&ti);
                            self.streams = ResolvedStream::Ready(rebuilt);

                            return Some(Effect::RefreshStreams);
                        }
                    }
                }
            }
            Event::HeatmapShaderInteraction(message) => {
                if let Content::ShaderHeatmap { chart: Some(c), .. } = &mut self.content {
                    c.update(message);
                }
            }
            Event::MiniTickersListInteraction(message) => {
                if let Some(Modal::MiniTickersList(ref mut mini_panel)) = self.modal
                    && let Some(action) = mini_panel.update(message)
                {
                    self.modal = Some(Modal::MiniTickersList(mini_panel.clone()));

                    let crate::modal::pane::mini_tickers_list::Action::RowSelected(sel) = action;
                    match sel {
                        crate::modal::pane::mini_tickers_list::RowSelection::Add(ti) => {
                            if let Content::Comparison(chart) = &mut self.content
                                && let Some(c) = chart
                            {
                                let rebuilt = c.add_ticker(&ti);
                                self.streams = ResolvedStream::Ready(rebuilt);
                                return Some(Effect::RefreshStreams);
                            }
                        }
                        crate::modal::pane::mini_tickers_list::RowSelection::Remove(ti) => {
                            if let Content::Comparison(chart) = &mut self.content
                                && let Some(c) = chart
                            {
                                let rebuilt = c.remove_ticker(&ti);
                                self.streams = ResolvedStream::Ready(rebuilt);
                                return Some(Effect::RefreshStreams);
                            }
                        }
                        crate::modal::pane::mini_tickers_list::RowSelection::Switch(ti) => {
                            return Some(Effect::SwitchTickersInGroup(ti));
                        }
                    }
                }
            }
        }
        None
    }

    fn view_controls(
        &'_ self,
        pane: pane_grid::Pane,
        total_panes: usize,
        is_maximized: bool,
        is_popout: bool,
    ) -> Element<'_, Message> {
        let modal_btn_style = |modal: Modal| {
            let is_active = self.modal == Some(modal);
            move |theme: &Theme, status: button::Status| {
                style::button::transparent(theme, status, is_active)
            }
        };

        let control_btn_style = |is_active: bool| {
            move |theme: &Theme, status: button::Status| {
                style::button::transparent(theme, status, is_active)
            }
        };

        let treat_as_starter =
            matches!(&self.content, Content::Starter) || !self.content.initialized();

        let tooltip_pos = tooltip::Position::Bottom;
        let mut buttons = row![];

        let show_modal = |modal: Modal| Message::PaneEvent(pane, Event::ShowModal(modal));

        if !treat_as_starter {
            buttons = buttons.push(button_with_tooltip(
                icon_text(Icon::Cog, 12),
                show_modal(Modal::Settings),
                None,
                tooltip_pos,
                modal_btn_style(Modal::Settings),
            ));

            let jump_to_latest = match &self.content {
                Content::Heatmap { chart: Some(_), .. } | Content::Kline { chart: Some(_), .. } => {
                    Some(Event::ChartInteraction(chart::Message::JumpToLatest))
                }
                Content::Comparison(Some(_)) => Some(Event::ComparisonChartInteraction(
                    chart::comparison::Message::JumpToLatest,
                )),
                Content::ShaderHeatmap { chart: Some(_), .. } => {
                    Some(Event::HeatmapShaderInteraction(
                        crate::widget::chart::heatmap::Message::JumpToLatest,
                    ))
                }
                _ => None,
            };

            if let Some(event) = jump_to_latest {
                buttons = buttons.push(button_with_tooltip(
                    text("Latest \u{2192}").size(crate::style::text_size::TINY),
                    Message::PaneEvent(pane, event),
                    Some("Jump to the latest price action"),
                    tooltip_pos,
                    control_btn_style(false),
                ));
            }
        }
        if !treat_as_starter
            && matches!(
                &self.content,
                Content::Heatmap { .. } | Content::Kline { .. } | Content::ShaderHeatmap { .. }
            )
        {
            buttons = buttons.push(button_with_tooltip(
                icon_text(Icon::ChartOutline, 12),
                show_modal(Modal::Indicators),
                Some("Indicators"),
                tooltip_pos,
                modal_btn_style(Modal::Indicators),
            ));
        }

        if is_popout {
            buttons = buttons.push(button_with_tooltip(
                icon_text(Icon::Popout, 12),
                Message::Merge,
                Some("Merge"),
                tooltip_pos,
                control_btn_style(is_popout),
            ));
        } else if total_panes > 1 {
            buttons = buttons.push(button_with_tooltip(
                icon_text(Icon::Popout, 12),
                Message::Popout,
                Some("Pop out"),
                tooltip_pos,
                control_btn_style(is_popout),
            ));
        }

        if total_panes > 1 {
            let (resize_icon, message) = if is_maximized {
                (Icon::ResizeSmall, Message::Restore)
            } else {
                (Icon::ResizeFull, Message::MaximizePane(pane))
            };

            buttons = buttons.push(button_with_tooltip(
                icon_text(resize_icon, 12),
                message,
                None,
                tooltip_pos,
                control_btn_style(is_maximized),
            ));

            buttons = buttons.push(button_with_tooltip(
                icon_text(Icon::Close, 12),
                Message::ClosePane(pane),
                None,
                tooltip_pos,
                control_btn_style(false),
            ));
        }

        buttons
            .padding(padding::right(4).left(4))
            .align_y(Alignment::Center)
            .height(Length::Fixed(32.0))
            .into()
    }

    fn compose_stack_view<'a, F>(
        &'a self,
        base: Element<'a, Message>,
        pane: pane_grid::Pane,
        indicator_modal: Option<Element<'a, Message>>,
        compact_controls: Option<Element<'a, Message>>,
        settings_modal: F,
        selected_tickers: Option<&'a [TickerInfo]>,
        tickers_table: &'a TickersTable,
    ) -> Element<'a, Message>
    where
        F: FnOnce() -> Element<'a, Message>,
    {
        let base =
            widget::toast::Manager::new(base, &self.notifications, Alignment::End, move |msg| {
                Message::PaneEvent(pane, Event::DeleteNotification(msg))
            })
            .into();

        let on_blur = Message::PaneEvent(pane, Event::HideModal);

        match &self.modal {
            Some(Modal::LinkGroup) => {
                let content = link_group_modal(pane, self.link_group);

                stack_modal(
                    base,
                    content,
                    on_blur,
                    padding::right(12).left(4),
                    Alignment::Start,
                )
            }
            Some(Modal::StreamModifier(modifier)) => stack_modal(
                base,
                modifier.view(self.stream_pair_kind()).map(move |message| {
                    Message::PaneEvent(pane, Event::StreamModifierChanged(message))
                }),
                Message::PaneEvent(pane, Event::HideModal),
                padding::right(12).left(48),
                Alignment::Start,
            ),
            Some(Modal::MiniTickersList(panel)) => {
                let mini_list = panel
                    .view(tickers_table, selected_tickers, self.stream_pair())
                    .map(move |msg| {
                        Message::PaneEvent(pane, Event::MiniTickersListInteraction(msg))
                    });

                let content: Element<_> = container(mini_list)
                    .max_width(260)
                    .padding(16)
                    .style(style::chart_modal)
                    .into();

                stack_modal(
                    base,
                    content,
                    Message::PaneEvent(pane, Event::HideModal),
                    padding::left(12),
                    Alignment::Start,
                )
            }
            Some(Modal::Settings) => stack_modal(
                base,
                settings_modal(),
                on_blur,
                padding::right(12).left(12),
                Alignment::End,
            ),
            Some(Modal::Indicators) => stack_modal(
                base,
                indicator_modal.unwrap_or_else(|| column![].into()),
                on_blur,
                padding::right(12).left(12),
                Alignment::End,
            ),
            Some(Modal::Controls) => stack_modal(
                base,
                if let Some(controls) = compact_controls {
                    controls
                } else {
                    column![].into()
                },
                on_blur,
                padding::left(12),
                Alignment::End,
            ),
            None => base,
        }
    }

    pub fn matches_stream(&self, stream: &StreamKind) -> bool {
        self.streams.matches_stream(stream)
    }

    fn show_modal_with_focus(&mut self, requested_modal: Modal) -> Option<Effect> {
        let should_toggle_close = match (&self.modal, &requested_modal) {
            (Some(Modal::StreamModifier(open)), Modal::StreamModifier(req)) => {
                open.view_mode == req.view_mode
            }
            (Some(open), req) => core::mem::discriminant(open) == core::mem::discriminant(req),
            _ => false,
        };

        if should_toggle_close {
            self.modal = None;
            return None;
        }

        let focus_widget_id = match &requested_modal {
            Modal::MiniTickersList(m) => Some(m.search_box_id.clone()),
            _ => None,
        };

        self.modal = Some(requested_modal);
        focus_widget_id.map(Effect::FocusWidget)
    }

    pub fn invalidate(&mut self, now: Instant) -> Option<Action> {
        match &mut self.content {
            Content::Heatmap { chart, .. } => chart
                .as_mut()
                .and_then(|c| c.invalidate(Some(now)).map(Action::Chart)),
            Content::Kline { chart, .. } => chart
                .as_mut()
                .and_then(|c| c.invalidate(Some(now)).map(Action::Chart)),
            Content::FootprintHistory(history) => history
                .as_mut()
                .and_then(|content| content.invalidate(Some(now)).map(Action::Chart)),
            Content::TimeAndSales(panel) => panel
                .as_mut()
                .and_then(|p| p.invalidate(Some(now)).map(Action::Panel)),
            Content::Ladder(panel) => panel
                .as_mut()
                .and_then(|p| p.invalidate(Some(now)).map(Action::Panel)),
            Content::Starter => None,
            Content::Comparison(chart) => chart
                .as_mut()
                .and_then(|c| c.invalidate(Some(now)).map(Action::Chart)),
            Content::ShaderHeatmap { chart, .. } => chart
                .as_mut()
                .and_then(|c| c.invalidate(Some(now)).map(Action::Chart)),
        }
    }

    /// Service periodic work while preserving render caches for chart panes
    /// whose market data and viewport have not changed.
    pub fn maintain(&mut self, now: Instant) -> Option<Action> {
        match &mut self.content {
            Content::Kline { chart, .. } => chart
                .as_mut()
                .and_then(|chart| chart.maintain(now).map(Action::Chart)),
            _ => self.invalidate(now),
        }
    }

    pub fn park_for_inactive_layout(&mut self) {
        if let Content::ShaderHeatmap { chart, .. } = &mut self.content {
            *chart = None;
            self.status = Status::Ready;
        }
    }

    pub fn update_interval(&self) -> Option<u64> {
        match &self.content {
            Content::Kline {
                chart: Some(chart), ..
            } if chart.has_pending_live_redraw() => {
                Some(crate::chart::kline::LIVE_REDRAW_INTERVAL_MS)
            }
            Content::Kline {
                chart: Some(chart), ..
            } if chart.needs_seed_backfill() => Some(100),
            Content::FootprintHistory(Some(history)) if history.has_pending_display_refresh() => {
                Some(100)
            }
            Content::Kline { .. } | Content::FootprintHistory(_) | Content::Comparison(_) => {
                Some(1000)
            }
            Content::Heatmap { chart, .. } => {
                if let Some(chart) = chart {
                    chart.basis_interval()
                } else {
                    None
                }
            }
            Content::Ladder(_) | Content::TimeAndSales(_) => Some(100),
            Content::ShaderHeatmap { .. } => None,
            Content::Starter => None,
        }
    }

    /// Returns `None` only when this pane needs a redraw on every rendered frame.
    pub fn tick_subscription_interval_ms(&self) -> Option<u64> {
        match &self.content {
            Content::ShaderHeatmap { chart: Some(_), .. } => None,
            Content::Heatmap {
                chart: Some(chart), ..
            } if chart.basis_interval().is_none() => Some(INITIALIZING_TICK_INTERVAL_MS),
            _ if !self.content.initialized() => Some(INITIALIZING_TICK_INTERVAL_MS),
            _ => Some(
                self.update_interval()
                    .unwrap_or(IDLE_TICK_INTERVAL_MS)
                    .max(1),
            ),
        }
    }

    pub fn last_tick(&self) -> Option<Instant> {
        self.content.last_tick()
    }

    pub fn tick(&mut self, now: Instant) -> Option<Action> {
        let invalidate_interval: Option<u64> = self.update_interval();
        let last_tick: Option<Instant> = self.last_tick();

        if let Some(streams) = self.streams.due_streams_to_resolve(now) {
            return Some(Action::ResolveStreams(streams));
        }

        if !self.content.initialized() {
            return Some(Action::ResolveContent);
        }

        match (invalidate_interval, last_tick) {
            (Some(interval_ms), Some(previous_tick_time)) => {
                if interval_ms > 0 {
                    let interval_duration = std::time::Duration::from_millis(interval_ms);
                    if now.duration_since(previous_tick_time) >= interval_duration {
                        return self.maintain(now);
                    }
                }
            }
            (Some(interval_ms), None) => {
                if interval_ms > 0 {
                    return self.maintain(now);
                }
            }
            (None, _) => {
                return self.maintain(now);
            }
        }

        None
    }

    pub fn unique_id(&self) -> uuid::Uuid {
        self.id
    }

    pub fn apply_synced_settings(
        &mut self,
        studies: &Option<data::chart::Study>,
        clusters: &Option<data::chart::kline::ClusterKind>,
    ) {
        if let Some(studies) = studies {
            self.content.update_studies(studies.clone());
        }
        if let Some(cluster_kind) = clusters
            && let Content::Kline { chart, kind, .. } = &mut self.content
            && let Some(c) = chart
        {
            c.set_cluster_kind(*cluster_kind);
            *kind = c.kind.clone();
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            modal: None,
            content: Content::Starter,
            settings: Settings::default(),
            streams: ResolvedStream::waiting(vec![]),
            notifications: vec![],
            status: Status::Ready,
            link_group: None,
        }
    }
}

#[derive(Default)]
pub enum Content {
    #[default]
    Starter,
    Heatmap {
        chart: Option<HeatmapChart>,
        indicators: Vec<HeatmapIndicator>,
        layout: data::chart::ViewConfig,
        studies: Vec<data::chart::heatmap::HeatmapStudy>,
    },
    ShaderHeatmap {
        chart: Option<Box<HeatmapShader>>,
        indicators: Vec<HeatmapIndicator>,
        studies: Vec<data::chart::heatmap::HeatmapStudy>,
    },
    Kline {
        chart: Option<KlineChart>,
        indicators: Vec<KlineIndicator>,
        layout: data::chart::ViewConfig,
        kind: data::chart::KlineChartKind,
    },
    FootprintHistory(Option<FootprintHistory>),
    TimeAndSales(Option<TimeAndSales>),
    Ladder(Option<Ladder>),
    Comparison(Option<ComparisonChart>),
}

impl Content {
    fn new_heatmap(
        current_content: &Content,
        ticker_info: TickerInfo,
        settings: &Settings,
        price_step: exchange::unit::PriceStep,
    ) -> Self {
        let (enabled_indicators, layout, prev_studies) = if let Content::Heatmap {
            chart,
            indicators,
            studies,
            layout,
        } = current_content
        {
            (
                indicators.clone(),
                chart
                    .as_ref()
                    .map(|c| c.chart_layout())
                    .unwrap_or(layout.clone()),
                chart
                    .as_ref()
                    .map_or(studies.clone(), |c| c.studies.clone()),
            )
        } else {
            (
                vec![HeatmapIndicator::Volume],
                ViewConfig {
                    splits: vec![],
                    autoscale: Some(data::chart::Autoscale::CenterLatest),
                },
                vec![],
            )
        };

        let basis = settings
            .selected_basis
            .unwrap_or_else(|| Basis::default_heatmap_time(Some(ticker_info)));
        let config = settings.visual_config.clone().and_then(|cfg| cfg.heatmap());

        let chart = HeatmapChart::new(
            layout.clone(),
            basis,
            price_step,
            &enabled_indicators,
            ticker_info,
            config,
            prev_studies.clone(),
        );

        Content::Heatmap {
            chart: Some(chart),
            indicators: enabled_indicators,
            layout,
            studies: prev_studies,
        }
    }

    fn new_kline(
        content_kind: ContentKind,
        current_content: &Content,
        ticker_info: TickerInfo,
        settings: &Settings,
        step: exchange::unit::PriceStep,
    ) -> Self {
        let (prev_indis, prev_layout, prev_kind_opt) = if let Content::Kline {
            chart,
            indicators,
            kind,
            layout,
        } = current_content
        {
            (
                Some(indicators.clone()),
                Some(chart.as_ref().map_or(layout.clone(), |c| c.chart_layout())),
                Some(chart.as_ref().map_or(kind.clone(), |c| c.kind().clone())),
            )
        } else {
            (None, None, None)
        };

        let preserve_indicators = matches!(
            (content_kind, prev_kind_opt.as_ref()),
            (
                ContentKind::FootprintChart,
                Some(data::chart::KlineChartKind::Footprint { .. })
            ) | (
                ContentKind::RenkoChart,
                Some(data::chart::KlineChartKind::Renko { .. })
            ) | (
                ContentKind::TpoChart,
                Some(data::chart::KlineChartKind::Tpo { .. })
            ) | (
                ContentKind::CandlestickChart,
                Some(data::chart::KlineChartKind::Candles)
            )
        );

        let (default_tf, determined_chart_kind) = match content_kind {
            ContentKind::FootprintChart => (
                Timeframe::M5,
                prev_kind_opt
                    .filter(|k| matches!(k, data::chart::KlineChartKind::Footprint { .. }))
                    .unwrap_or_else(|| data::chart::KlineChartKind::Footprint {
                        clusters: data::chart::kline::ClusterKind::default(),
                        scaling: data::chart::kline::ClusterScaling::default(),
                        studies: vec![],
                    }),
            ),
            ContentKind::RenkoChart => (
                Timeframe::M15,
                prev_kind_opt
                    .filter(|k| matches!(k, data::chart::KlineChartKind::Renko { .. }))
                    .unwrap_or_else(|| data::chart::KlineChartKind::Renko {
                        config: data::chart::kline::RenkoConfig::default(),
                    }),
            ),
            ContentKind::TpoChart => (
                Timeframe::M30,
                prev_kind_opt
                    .filter(|k| matches!(k, data::chart::KlineChartKind::Tpo { .. }))
                    .unwrap_or_else(|| data::chart::KlineChartKind::Tpo {
                        config: data::chart::tpo::Config::default(),
                    }),
            ),
            ContentKind::CandlestickChart => (Timeframe::M15, data::chart::KlineChartKind::Candles),
            _ => unreachable!("invalid content kind for kline chart"),
        };
        let determined_chart_kind = match determined_chart_kind {
            data::chart::KlineChartKind::Tpo { config } => data::chart::KlineChartKind::Tpo {
                config: config.normalized(),
            },
            kind => kind,
        };

        let basis = settings.selected_basis.unwrap_or(Basis::Time(default_tf));

        let enabled_indicators = {
            let available = KlineIndicator::for_market(ticker_info.market_type());
            prev_indis.filter(|_| preserve_indicators).map_or_else(
                || match &determined_chart_kind {
                    data::chart::KlineChartKind::Footprint { .. } => {
                        vec![KlineIndicator::BarAnalysis]
                    }
                    data::chart::KlineChartKind::Renko { .. } => {
                        vec![KlineIndicator::CumulativeDelta]
                    }
                    data::chart::KlineChartKind::Tpo { .. } => vec![],
                    data::chart::KlineChartKind::Candles => vec![KlineIndicator::Volume],
                },
                |indis| {
                    indis
                        .into_iter()
                        .filter(|i| {
                            available.contains(i) && determined_chart_kind.allows_indicator(*i)
                        })
                        .collect()
                },
            )
        };

        let splits = {
            let main_chart_split: f32 = 0.8;
            let mut splits_vec = vec![main_chart_split];
            let num_indicators = enabled_indicators
                .iter()
                .filter(|indicator| !indicator.is_overlay())
                .count();

            if num_indicators > 1 {
                let indicator_total_height_ratio = 1.0 - main_chart_split;
                let height_per_indicator_pane =
                    indicator_total_height_ratio / num_indicators as f32;

                let mut current_split_pos = main_chart_split;
                for _ in 0..(num_indicators - 1) {
                    current_split_pos += height_per_indicator_pane;
                    splits_vec.push(current_split_pos);
                }
            }
            splits_vec
        };

        let layout = prev_layout
            .filter(|l| l.splits.len() == splits.len())
            .unwrap_or(ViewConfig {
                splits,
                autoscale: Some(data::chart::Autoscale::FitToVisible),
            });
        let visual_config = settings.visual_config.as_ref().and_then(|cfg| cfg.kline());

        let chart = KlineChart::new(
            layout.clone(),
            basis,
            step,
            &[],
            vec![],
            &enabled_indicators,
            ticker_info,
            &determined_chart_kind,
            visual_config,
        );

        Content::Kline {
            chart: Some(chart),
            indicators: enabled_indicators,
            layout,
            kind: determined_chart_kind,
        }
    }

    fn placeholder(kind: ContentKind) -> Self {
        match kind {
            ContentKind::Starter => Content::Starter,
            ContentKind::FootprintHistory => Content::FootprintHistory(None),
            ContentKind::CandlestickChart => Content::Kline {
                chart: None,
                indicators: vec![KlineIndicator::Volume],
                kind: data::chart::KlineChartKind::Candles,
                layout: ViewConfig {
                    splits: vec![],
                    autoscale: Some(data::chart::Autoscale::FitToVisible),
                },
            },
            ContentKind::RenkoChart => Content::Kline {
                chart: None,
                indicators: vec![KlineIndicator::CumulativeDelta],
                kind: data::chart::KlineChartKind::Renko {
                    config: data::chart::kline::RenkoConfig::default(),
                },
                layout: ViewConfig {
                    splits: vec![0.8],
                    autoscale: Some(data::chart::Autoscale::FitToVisible),
                },
            },
            ContentKind::TpoChart => Content::Kline {
                chart: None,
                indicators: vec![],
                kind: data::chart::KlineChartKind::Tpo {
                    config: data::chart::tpo::Config::default(),
                },
                layout: ViewConfig {
                    splits: vec![],
                    autoscale: Some(data::chart::Autoscale::FitToVisible),
                },
            },
            ContentKind::FootprintChart => Content::Kline {
                chart: None,
                indicators: vec![KlineIndicator::BarAnalysis],
                kind: data::chart::KlineChartKind::Footprint {
                    clusters: data::chart::kline::ClusterKind::default(),
                    scaling: data::chart::kline::ClusterScaling::default(),
                    studies: vec![],
                },
                layout: ViewConfig {
                    splits: vec![],
                    autoscale: Some(data::chart::Autoscale::FitToVisible),
                },
            },
            ContentKind::ShaderHeatmap => Content::ShaderHeatmap {
                chart: None,
                indicators: vec![HeatmapIndicator::Volume],
                studies: vec![data::chart::heatmap::HeatmapStudy::VolumeProfile(
                    data::chart::heatmap::ProfileKind::default(),
                )],
            },
            ContentKind::HeatmapChart => Content::Heatmap {
                chart: None,
                indicators: vec![HeatmapIndicator::Volume],
                studies: vec![],
                layout: ViewConfig {
                    splits: vec![],
                    autoscale: Some(data::chart::Autoscale::CenterLatest),
                },
            },
            ContentKind::ComparisonChart => Content::Comparison(None),
            ContentKind::TimeAndSales => Content::TimeAndSales(None),
            ContentKind::Ladder => Content::Ladder(None),
        }
    }

    pub fn last_tick(&self) -> Option<Instant> {
        match self {
            Content::Heatmap { chart, .. } => Some(chart.as_ref()?.last_update()),
            Content::Kline { chart, .. } => Some(chart.as_ref()?.last_update()),
            Content::TimeAndSales(panel) => Some(panel.as_ref()?.last_update()),
            Content::Ladder(panel) => Some(panel.as_ref()?.last_update()),
            Content::Comparison(chart) => Some(chart.as_ref()?.last_update()),
            Content::FootprintHistory(history) => Some(history.as_ref()?.last_update()),
            Content::Starter => None,
            Content::ShaderHeatmap { chart, .. } => Some(chart.as_ref()?.last_tick?),
        }
    }

    pub fn chart_kind(&self) -> Option<data::chart::KlineChartKind> {
        match self {
            Content::Kline { chart, .. } => Some(chart.as_ref()?.kind().clone()),
            _ => None,
        }
    }

    pub fn toggle_indicator(&mut self, indicator: UiIndicator) {
        match (self, indicator) {
            (
                Content::Heatmap {
                    chart, indicators, ..
                },
                UiIndicator::Heatmap(ind),
            ) => {
                let Some(chart) = chart else {
                    return;
                };

                if indicators.contains(&ind) {
                    indicators.retain(|i| i != &ind);
                } else {
                    indicators.push(ind);
                }
                chart.toggle_indicator(ind);
            }
            (
                Content::Kline {
                    chart,
                    indicators,
                    kind,
                    ..
                },
                UiIndicator::Kline(ind),
            ) => {
                let Some(chart) = chart else {
                    return;
                };
                let is_enabled = indicators.contains(&ind);
                if !is_enabled && !kind.allows_indicator(ind) {
                    return;
                }
                if !is_enabled
                    && ind == KlineIndicator::LiquidityHeatmap
                    && !chart.allows_liquidity_heatmap()
                {
                    return;
                }

                if is_enabled {
                    indicators.retain(|i| i != &ind);
                } else {
                    indicators.push(ind);
                }
                chart.toggle_indicator(ind);
            }
            (
                Content::ShaderHeatmap {
                    chart, indicators, ..
                },
                UiIndicator::Heatmap(ind),
            ) => {
                let Some(chart) = chart else {
                    return;
                };

                if indicators.contains(&ind) {
                    indicators.retain(|i| i != &ind);
                } else {
                    indicators.push(ind);
                }
                chart.toggle_indicator(ind);
            }
            _ => panic!("indicator toggle on {indicator:?} pane",),
        }
    }

    pub fn reorder_indicators(&mut self, event: &column_drag::DragEvent) {
        match self {
            Content::Heatmap { indicators, .. } => column_drag::reorder_vec(indicators, event),
            Content::Kline { indicators, .. } => column_drag::reorder_vec(indicators, event),
            Content::TimeAndSales(_)
            | Content::Ladder(_)
            | Content::Starter
            | Content::FootprintHistory(_)
            | Content::Comparison(_)
            | Content::ShaderHeatmap { .. } => {
                panic!("indicator reorder on {} pane", self)
            }
        }
    }

    pub fn change_visual_config(&mut self, config: VisualConfig) {
        match (self, config) {
            (Content::Kline { chart: Some(c), .. }, VisualConfig::Kline(cfg)) => {
                c.set_visual_config(cfg);
            }
            (Content::Heatmap { chart: Some(c), .. }, VisualConfig::Heatmap(cfg)) => {
                c.set_visual_config(cfg);
            }
            (Content::ShaderHeatmap { chart: Some(c), .. }, VisualConfig::Heatmap(cfg)) => {
                c.set_visual_config(cfg);
            }
            (Content::Comparison(Some(chart)), VisualConfig::Comparison(cfg)) => {
                chart.config = cfg;
            }
            (Content::TimeAndSales(Some(panel)), VisualConfig::TimeAndSales(cfg)) => {
                panel.config = cfg;
            }
            (Content::Ladder(Some(panel)), VisualConfig::Ladder(cfg)) => {
                panel.config = cfg;
            }
            _ => {}
        }
    }

    pub fn studies(&self) -> Option<data::chart::Study> {
        match &self {
            Content::Heatmap { studies, .. } => Some(data::chart::Study::Heatmap(studies.clone())),
            Content::ShaderHeatmap { studies, .. } => {
                Some(data::chart::Study::Heatmap(studies.clone()))
            }
            Content::Kline { kind, .. } => {
                if let data::chart::KlineChartKind::Footprint { studies, .. } = kind {
                    Some(data::chart::Study::Footprint(studies.clone()))
                } else {
                    None
                }
            }
            Content::TimeAndSales(_)
            | Content::Ladder(_)
            | Content::Starter
            | Content::FootprintHistory(_)
            | Content::Comparison(_) => None,
        }
    }

    pub fn clusters(&self) -> Option<data::chart::kline::ClusterKind> {
        match self {
            Content::Kline {
                kind: data::chart::KlineChartKind::Footprint { clusters, .. },
                ..
            } => Some(*clusters),
            _ => None,
        }
    }

    pub fn update_studies(&mut self, studies: data::chart::Study) {
        match (self, studies) {
            (
                Content::Heatmap {
                    chart,
                    studies: previous,
                    ..
                },
                data::chart::Study::Heatmap(studies),
            ) => {
                chart
                    .as_mut()
                    .expect("heatmap chart not initialized")
                    .studies = studies.clone();
                *previous = studies;
            }
            (
                Content::ShaderHeatmap {
                    chart,
                    studies: previous,
                    ..
                },
                data::chart::Study::Heatmap(studies),
            ) => {
                chart
                    .as_mut()
                    .expect("shader heatmap chart not initialized")
                    .studies = studies.clone();
                *previous = studies;
            }
            (Content::Kline { chart, kind, .. }, data::chart::Study::Footprint(studies)) => {
                let chart = chart.as_mut().expect("kline chart not initialized");
                chart.set_studies(studies.clone());
                if let data::chart::KlineChartKind::Footprint {
                    studies: k_studies, ..
                } = kind
                {
                    *k_studies = chart.studies().unwrap_or_default();
                }
            }
            _ => {}
        }
    }

    pub fn kind(&self) -> ContentKind {
        match self {
            Content::Heatmap { .. } => ContentKind::HeatmapChart,
            Content::Kline { kind, .. } => match kind {
                data::chart::KlineChartKind::Footprint { .. } => ContentKind::FootprintChart,
                data::chart::KlineChartKind::Renko { .. } => ContentKind::RenkoChart,
                data::chart::KlineChartKind::Tpo { .. } => ContentKind::TpoChart,
                data::chart::KlineChartKind::Candles => ContentKind::CandlestickChart,
            },
            Content::TimeAndSales(_) => ContentKind::TimeAndSales,
            Content::Ladder(_) => ContentKind::Ladder,
            Content::Comparison(_) => ContentKind::ComparisonChart,
            Content::FootprintHistory(_) => ContentKind::FootprintHistory,
            Content::Starter => ContentKind::Starter,
            Content::ShaderHeatmap { .. } => ContentKind::ShaderHeatmap,
        }
    }

    pub fn update_theme(&mut self, theme: &iced_core::Theme) {
        match self {
            Content::ShaderHeatmap { chart: Some(c), .. } => c.update_theme(theme),
            Content::Kline { chart: Some(c), .. } => c.update_theme(theme),
            _ => {}
        }
    }

    fn initialized(&self) -> bool {
        match self {
            Content::Heatmap { chart, .. } => chart.is_some(),
            Content::ShaderHeatmap { chart, .. } => chart.is_some(),
            Content::Kline { chart, .. } => chart.is_some(),
            Content::FootprintHistory(history) => history.is_some(),
            Content::TimeAndSales(panel) => panel.is_some(),
            Content::Ladder(panel) => panel.is_some(),
            Content::Comparison(chart) => chart.is_some(),
            Content::Starter => true,
        }
    }

    pub fn allows_indicator(&self, indicator: UiIndicator) -> bool {
        match (self, indicator) {
            (Content::Kline { kind, .. }, UiIndicator::Kline(indicator)) => {
                kind.allows_indicator(indicator)
            }
            (Content::Heatmap { .. } | Content::ShaderHeatmap { .. }, UiIndicator::Heatmap(_)) => {
                true
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind())
    }
}

impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Content::Starter, Content::Starter)
                | (Content::Heatmap { .. }, Content::Heatmap { .. })
                | (Content::Kline { .. }, Content::Kline { .. })
                | (Content::FootprintHistory(_), Content::FootprintHistory(_))
                | (Content::TimeAndSales(_), Content::TimeAndSales(_))
                | (Content::Ladder(_), Content::Ladder(_))
        )
    }
}

fn link_group_modal<'a>(
    pane: pane_grid::Pane,
    selected_group: Option<LinkGroup>,
) -> Element<'a, Message> {
    let mut grid = column![].spacing(4);
    let rows = LinkGroup::ALL.chunks(3);

    for row_groups in rows {
        let mut button_row = row![].spacing(4);

        for &group in row_groups {
            let is_selected = selected_group == Some(group);
            let btn_content = text(group.to_string()).font(style::AZERET_MONO);

            let btn = if is_selected {
                button_with_tooltip(
                    btn_content.align_x(iced::Alignment::Center),
                    Message::SwitchLinkGroup(pane, None),
                    Some("Unlink"),
                    tooltip::Position::Bottom,
                    move |theme, status| style::button::menu_body(theme, status, true),
                )
            } else {
                button(btn_content.align_x(iced::Alignment::Center))
                    .on_press(Message::SwitchLinkGroup(pane, Some(group)))
                    .style(move |theme, status| style::button::menu_body(theme, status, false))
                    .into()
            };

            button_row = button_row.push(btn);
        }

        grid = grid.push(button_row);
    }

    container(grid)
        .max_width(240)
        .padding(16)
        .style(style::chart_modal)
        .into()
}

fn ticksize_modifier<'a>(
    id: pane_grid::Pane,
    price_step: PriceStep,
    min_ticksize: Option<exchange::unit::MinTicksize>,
    multiplier: TickMultiplier,
    modifier: Option<modal::stream::Modifier>,
    kind: ModifierKind,
    exchange: Option<exchange::adapter::Exchange>,
) -> Element<'a, Message> {
    let modifier_modal =
        Modal::StreamModifier(modal::stream::Modifier::new(kind).with_ticksize_view(
            price_step,
            min_ticksize,
            multiplier,
            exchange,
        ));

    let is_active = modifier.is_some_and(|m| {
        matches!(
            m.view_mode,
            modal::stream::ViewMode::TicksizeSelection { .. }
        )
    });

    button(text(multiplier.to_string()).align_y(Alignment::Center))
        .style(move |theme, status| style::button::modifier(theme, status, !is_active))
        .on_press(Message::PaneEvent(id, Event::ShowModal(modifier_modal)))
        .height(widget::PANE_CONTROL_BTN_HEIGHT)
        .into()
}

#[allow(clippy::too_many_arguments)]
fn footprint_history_modifier(
    id: pane_grid::Pane,
    block_step: PriceStep,
    base_step: PriceStep,
    min_ticksize: Option<exchange::unit::MinTicksize>,
    multiplier: TickMultiplier,
    modifier: Option<modal::stream::Modifier>,
    exchange: Option<exchange::adapter::Exchange>,
) -> Element<'static, Message> {
    let kind = ModifierKind::Footprint(Basis::Tick(data::aggr::TickCount(1)), multiplier);
    let modifier_modal =
        Modal::StreamModifier(modal::stream::Modifier::new(kind).with_ticksize_view(
            base_step,
            min_ticksize,
            multiplier,
            exchange,
        ));
    let is_active = modifier.is_some_and(|value| {
        matches!(
            value.view_mode,
            modal::stream::ViewMode::TicksizeSelection { .. }
        )
    });

    button(text(format!("Block {}", block_step.to_ui_string())).align_y(Alignment::Center))
        .style(move |theme, status| style::button::modifier(theme, status, !is_active))
        .on_press(Message::PaneEvent(id, Event::ShowModal(modifier_modal)))
        .height(widget::PANE_CONTROL_BTN_HEIGHT)
        .into()
}

fn basis_modifier<'a>(
    id: pane_grid::Pane,
    selected_basis: Basis,
    modifier: Option<modal::stream::Modifier>,
    kind: ModifierKind,
) -> Element<'a, Message> {
    let modifier_modal = Modal::StreamModifier(
        modal::stream::Modifier::new(kind).with_view_mode(modal::stream::ViewMode::BasisSelection),
    );

    let is_active =
        modifier.is_some_and(|m| m.view_mode == modal::stream::ViewMode::BasisSelection);

    button(text(selected_basis.to_string()).align_y(Alignment::Center))
        .style(move |theme, status| style::button::modifier(theme, status, !is_active))
        .on_press(Message::PaneEvent(id, Event::ShowModal(modifier_modal)))
        .height(widget::PANE_CONTROL_BTN_HEIGHT)
        .into()
}

fn renko_modifier(
    id: pane_grid::Pane,
    config: data::chart::kline::RenkoConfig,
    is_active: bool,
) -> Element<'static, Message> {
    button(text(config.to_string()).align_y(Alignment::Center))
        .style(move |theme, status| style::button::modifier(theme, status, !is_active))
        .on_press(Message::PaneEvent(id, Event::ShowModal(Modal::Settings)))
        .height(widget::PANE_CONTROL_BTN_HEIGHT)
        .into()
}

fn tpo_modifier(
    id: pane_grid::Pane,
    config: data::chart::tpo::Config,
    is_active: bool,
) -> Element<'static, Message> {
    button(text(config.to_string()).align_y(Alignment::Center))
        .style(move |theme, status| style::button::modifier(theme, status, !is_active))
        .on_press(Message::PaneEvent(id, Event::ShowModal(Modal::Settings)))
        .height(widget::PANE_CONTROL_BTN_HEIGHT)
        .into()
}

fn by_basis_default<T>(
    basis: Option<Basis>,
    default_tf: Timeframe,
    on_time: impl FnOnce(Timeframe) -> T,
    on_tick: impl FnOnce() -> T,
) -> T {
    match basis.unwrap_or(Basis::Time(default_tf)) {
        Basis::Time(tf) => on_time(tf),
        Basis::Tick(_) => on_tick(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;
    use exchange::{
        Ticker, UnixMs,
        depth::Depth,
        unit::{Price, Qty},
    };

    fn ticker_info(exchange: Exchange, symbol: &str, min_ticksize: f32) -> TickerInfo {
        TickerInfo::new(Ticker::new(symbol, exchange), min_ticksize, 0.001, None)
    }

    #[test]
    fn tpo_source_toggles_rebuild_streams_and_support_single_venue_mode() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut state = State::default();

        assert_eq!(
            state
                .set_content_and_streams(vec![binance, bybit, hyperliquid], ContentKind::TpoChart,)
                .len(),
            3
        );

        assert!(matches!(
            state.update(Event::AggregateSourceToggled(binance, false)),
            Some(Effect::RefreshStreams)
        ));
        assert!(matches!(
            state.update(Event::AggregateSourceToggled(hyperliquid, false)),
            Some(Effect::RefreshStreams)
        ));

        let active = state
            .streams
            .ready_iter()
            .expect("ready streams")
            .map(StreamKind::ticker_info)
            .collect::<Vec<_>>();
        assert_eq!(active, vec![bybit]);
        assert_eq!(state.settings.aggregate_sources, Some(vec![bybit.ticker]));

        assert!(
            state
                .update(Event::AggregateSourceToggled(bybit, false))
                .is_none()
        );
        assert_eq!(state.notifications.len(), 1);
    }

    #[test]
    fn tpo_can_enable_source_metadata_that_arrived_after_chart_creation() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let mut state = State::default();

        state.set_content_and_streams(vec![binance], ContentKind::TpoChart);
        assert!(matches!(
            state.update(Event::AggregateSourceToggled(bybit, true)),
            Some(Effect::RefreshStreams)
        ));

        let active = state
            .streams
            .ready_iter()
            .expect("ready streams")
            .map(StreamKind::ticker_info)
            .collect::<Vec<_>>();
        assert_eq!(active, vec![binance, bybit]);
    }

    #[test]
    fn tpo_keeps_shared_price_grid_in_hyperliquid_only_mode() {
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut state = State::default();

        state.set_content_and_streams(vec![hyperliquid], ContentKind::TpoChart);

        let Content::Kline {
            chart: Some(chart), ..
        } = &state.content
        else {
            panic!("TPO chart initialized");
        };
        assert_eq!(chart.tick_size().to_ui_string(), "0.1");
    }

    #[test]
    fn footprint_uses_all_three_btc_perpetual_trade_sources() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut state = State::default();

        let streams = state.set_content_and_streams(
            vec![binance, bybit, hyperliquid],
            ContentKind::FootprintChart,
        );

        assert_eq!(
            streams
                .iter()
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            3
        );
        assert_eq!(
            streams
                .iter()
                .filter(|stream| matches!(stream, StreamKind::Kline { .. }))
                .count(),
            1
        );
        let Content::Kline {
            chart: Some(chart), ..
        } = &state.content
        else {
            panic!("Footprint chart initialized");
        };
        assert_eq!(chart.feed().sources(), &[binance, bybit, hyperliquid]);
        // Shared BTC grid is 0.1; default footprint grouping is 50x.
        assert_eq!(chart.tick_size().to_ui_string(), "5");
        assert!(matches!(
            state.stream_pair_kind(),
            Some(StreamPairKind::MultiSource(sources)) if sources == vec![binance, bybit, hyperliquid]
        ));

        assert!(matches!(
            state.update(Event::AggregateSourceToggled(hyperliquid, false)),
            Some(Effect::RefreshStreams)
        ));
        assert!(matches!(
            state.content,
            Content::Kline {
                kind: data::chart::KlineChartKind::Footprint { .. },
                ..
            }
        ));
    }

    #[test]
    fn aggregated_footprint_applies_saved_tick_multiplier_to_the_shared_grid() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut state = State::from_config(
            Content::placeholder(ContentKind::FootprintChart),
            vec![],
            Settings {
                tick_multiply: Some(TickMultiplier(200)),
                selected_basis: Some(Basis::Time(Timeframe::M30)),
                ..Settings::default()
            },
            None,
        );

        state.set_content_and_streams(
            vec![binance, bybit, hyperliquid],
            ContentKind::FootprintChart,
        );

        let Content::Kline {
            chart: Some(chart),
            indicators,
            ..
        } = &state.content
        else {
            panic!("Footprint chart initialized");
        };
        assert_eq!(chart.tick_size().to_ui_string(), "20");
        assert!(!indicators.contains(&KlineIndicator::LiquidityHeatmap));
    }

    #[test]
    fn restored_aggregate_footprint_keeps_a_kline_stream() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let content = Content::placeholder(ContentKind::FootprintChart);
        let state = State::from_config(
            content,
            vec![PersistStreamKind::AggregateTrades {
                feed: AggregateFeedId::BtcUsdtPerpetual,
            }],
            Settings {
                selected_basis: Some(Basis::Time(Timeframe::M30)),
                aggregate_feed: Some(AggregateFeedId::BtcUsdtPerpetual),
                ..Settings::default()
            },
            None,
        );
        let ResolvedStream::Waiting { streams, .. } = &state.streams else {
            panic!("restored streams wait for metadata");
        };
        assert!(
            streams.iter().any(
                |stream| matches!(stream, PersistStreamKind::Kline { ticker, timeframe }
                    if *ticker == binance.ticker && *timeframe == Timeframe::M30)
            ),
            "time-based footprint must keep a kline stream or the pane stays empty: {streams:?}"
        );
    }

    #[test]
    fn footprint_aggregates_equivalent_linear_perps_outside_the_btc_catalog() {
        let binance = ticker_info(Exchange::BinanceLinear, "ETHUSDT", 0.01);
        let bybit = ticker_info(Exchange::BybitLinear, "ETHUSDT", 0.01);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "ETH", 0.1);
        let mut state = State::default();

        let streams = state.set_content_and_streams(
            vec![binance, bybit, hyperliquid],
            ContentKind::FootprintChart,
        );

        assert_eq!(
            streams
                .iter()
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            3
        );
        let Content::Kline {
            chart: Some(chart), ..
        } = &state.content
        else {
            panic!("Footprint chart initialized");
        };
        assert_eq!(chart.feed().sources(), &[binance, bybit, hyperliquid]);
        assert!(chart.feed().id().is_none());
    }

    #[test]
    fn footprint_history_streams_are_toggleable_and_hidden_from_main_chart_identity() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 0.1);
        let mut state = State::default();
        state.set_content_and_streams(vec![binance], ContentKind::CandlestickChart);

        assert!(matches!(
            state.update(Event::ToggleIndicator(
                UiIndicator::Kline(KlineIndicator::FootprintHistory),
                vec![binance, bybit, hyperliquid],
            )),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            3
        );
        assert!(matches!(
            state.stream_pair_kind(),
            Some(StreamPairKind::SingleSource(source)) if source == binance
        ));

        state.update(Event::ToggleIndicator(
            UiIndicator::Kline(KlineIndicator::FootprintHistory),
            vec![binance, bybit, hyperliquid],
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn daily_delta_reuses_trade_history_streams_and_aggregation() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 0.1);
        let mut state = State::default();
        state.set_content_and_streams(vec![binance], ContentKind::CandlestickChart);

        assert!(matches!(
            state.update(Event::ToggleIndicator(
                UiIndicator::Kline(KlineIndicator::DailyDelta),
                vec![binance, bybit, hyperliquid],
            )),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            3
        );
        assert!(matches!(
            state.stream_pair_kind(),
            Some(StreamPairKind::SingleSource(source)) if source == binance
        ));

        let Content::Kline {
            indicators,
            chart: Some(chart),
            ..
        } = &state.content
        else {
            panic!("candlestick chart initialized");
        };
        assert!(indicators.contains(&KlineIndicator::DailyDelta));
        assert!(chart.footprint_history_aggregate());
        assert_eq!(chart.footprint_history_sources().len(), 3);
    }

    #[test]
    fn restored_daily_delta_resolves_selected_bybit_and_hyperliquid_trade_streams() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 0.1);
        let mut content = Content::placeholder(ContentKind::CandlestickChart);
        let Content::Kline { indicators, .. } = &mut content else {
            panic!("candlestick placeholder");
        };
        indicators.push(KlineIndicator::DailyDelta);

        let mut state = State::from_config(
            content,
            vec![PersistStreamKind::Kline {
                ticker: binance.ticker,
                timeframe: Timeframe::M30,
            }],
            Settings {
                footprint_history_sources: Some(vec![bybit.ticker, hyperliquid.ticker]),
                footprint_history_aggregate: true,
                ..Settings::default()
            },
            None,
        );
        let ResolvedStream::Waiting { streams, .. } = &state.streams else {
            panic!("persisted streams wait for metadata");
        };
        let restored_sources = streams
            .iter()
            .filter_map(|stream| match stream {
                PersistStreamKind::Trades { ticker } => Some(*ticker),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(restored_sources, vec![bybit.ticker, hyperliquid.ticker]);

        let metadata = [binance, bybit, hyperliquid];
        let resolved = streams
            .clone()
            .into_iter()
            .flat_map(|stream| {
                stream
                    .into_stream_kinds(|ticker| {
                        metadata
                            .iter()
                            .copied()
                            .find(|info| info.ticker.same_market(ticker))
                    })
                    .expect("configured metadata resolves")
            })
            .collect::<Vec<_>>();
        state.streams = ResolvedStream::Ready(resolved);
        let Some(StreamPairKind::MultiSource(tickers)) = state.stream_pair_kind() else {
            panic!("restored sources are available during chart initialization");
        };
        state.set_content_and_streams(tickers, ContentKind::CandlestickChart);

        let Content::Kline {
            chart: Some(chart), ..
        } = &state.content
        else {
            panic!("restored candlestick chart initialized");
        };
        assert_eq!(chart.footprint_history_sources(), &[bybit, hyperliquid]);
    }

    #[test]
    fn restored_liquidity_heatmap_rebuilds_selected_depth_streams() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut content = Content::placeholder(ContentKind::CandlestickChart);
        let Content::Kline { indicators, .. } = &mut content else {
            panic!("candlestick placeholder");
        };
        indicators.push(KlineIndicator::LiquidityHeatmap);

        let mut state = State::from_config(
            content,
            vec![PersistStreamKind::Kline {
                ticker: binance.ticker,
                timeframe: Timeframe::M30,
            }],
            Settings {
                liquidity_heatmap_sources: Some(vec![
                    binance.ticker,
                    bybit.ticker,
                    hyperliquid.ticker,
                ]),
                ..Settings::default()
            },
            None,
        );
        let ResolvedStream::Waiting { streams, .. } = &state.streams else {
            panic!("persisted streams wait for metadata");
        };
        let restored_sources = streams
            .iter()
            .filter_map(|stream| match stream {
                PersistStreamKind::Depth(depth) => Some(depth.ticker),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            restored_sources,
            vec![binance.ticker, bybit.ticker, hyperliquid.ticker]
        );

        let metadata = [binance, bybit, hyperliquid];
        let resolved = streams
            .clone()
            .into_iter()
            .flat_map(|stream| {
                stream
                    .into_stream_kinds(|ticker| {
                        metadata
                            .iter()
                            .copied()
                            .find(|info| info.ticker.same_market(ticker))
                    })
                    .expect("configured metadata resolves")
            })
            .collect::<Vec<_>>();
        state.streams = ResolvedStream::Ready(resolved);
        let Some(StreamPairKind::MultiSource(tickers)) = state.stream_pair_kind() else {
            panic!("restored depth sources are available during chart initialization");
        };
        state.set_content_and_streams(tickers, ContentKind::CandlestickChart);

        let Content::Kline {
            chart: Some(chart), ..
        } = &state.content
        else {
            panic!("restored candlestick chart initialized");
        };
        assert_eq!(
            chart.liquidity_heatmap_sources(),
            &[binance, bybit, hyperliquid]
        );
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Depth { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn previous_value_area_enables_without_trade_venue_streams() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let mut state = State::default();
        state.set_content_and_streams(vec![binance], ContentKind::CandlestickChart);

        // Previous Value Areas builds TPO profiles from OHLC bars, so enabling
        // it must not subscribe to any additional trade streams.
        assert!(
            state
                .update(Event::ToggleIndicator(
                    UiIndicator::Kline(KlineIndicator::PreviousValueArea),
                    vec![binance],
                ))
                .is_none()
        );
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            0
        );

        let Content::Kline {
            indicators,
            chart: Some(_),
            ..
        } = &state.content
        else {
            panic!("candlestick chart initialized");
        };
        assert!(indicators.contains(&KlineIndicator::PreviousValueArea));
    }

    #[test]
    fn vpvr_subscribes_to_the_pane_trade_stream_on_candles() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let mut state = State::default();
        state.set_content_and_streams(vec![binance], ContentKind::CandlestickChart);

        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            0
        );

        assert!(matches!(
            state.update(Event::ToggleIndicator(
                UiIndicator::Kline(KlineIndicator::VisibleRangeProfile),
                vec![binance],
            )),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            1
        );

        let Content::Kline {
            indicators,
            chart: Some(_),
            ..
        } = &state.content
        else {
            panic!("candlestick chart initialized");
        };
        assert!(indicators.contains(&KlineIndicator::VisibleRangeProfile));

        state.update(Event::ToggleIndicator(
            UiIndicator::Kline(KlineIndicator::VisibleRangeProfile),
            vec![binance],
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Trades { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn liquidity_heatmap_uses_three_depth_streams_without_changing_chart_identity() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 1.0);
        let mut state = State::default();
        state.set_content_and_streams(vec![binance], ContentKind::CandlestickChart);

        assert!(matches!(
            state.update(Event::ToggleIndicator(
                UiIndicator::Kline(KlineIndicator::LiquidityHeatmap),
                vec![binance, bybit, hyperliquid],
            )),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Depth { .. }))
                .count(),
            3
        );
        assert!(matches!(
            state.stream_pair_kind(),
            Some(StreamPairKind::SingleSource(source)) if source == binance
        ));

        let Content::Kline {
            indicators,
            chart: Some(chart),
            ..
        } = &state.content
        else {
            panic!("candlestick chart initialized");
        };
        assert!(indicators.contains(&KlineIndicator::LiquidityHeatmap));
        assert_eq!(chart.liquidity_heatmap_sources().len(), 3);
        assert!(chart.liquidity_heatmap_runtime_active());
        assert!(!chart.liquidity_heatmap_has_depth_data());

        let depth = Depth {
            bids: [(Price::from_f64(70_000.0), Qty::from_f64(2.0))]
                .into_iter()
                .collect(),
            asks: [(Price::from_f64(70_001.0), Qty::from_f64(3.0))]
                .into_iter()
                .collect(),
        };
        let Content::Kline {
            chart: Some(chart), ..
        } = &mut state.content
        else {
            panic!("candlestick chart initialized");
        };
        chart.insert_depth(binance, &depth, UnixMs::new(1_000));
        assert!(chart.liquidity_heatmap_has_depth_data());

        let mut config = chart.visual_config();
        config.liquidity_heatmap_order_size_filter = 125_000.0;
        chart.set_visual_config(config);
        assert_eq!(chart.liquidity_heatmap_order_size_filter(), Some(125_000.0));

        assert!(matches!(
            state.update(Event::LiquidityHeatmapSourceToggled(bybit, false)),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Depth { .. }))
                .count(),
            2
        );

        assert!(matches!(
            state.update(Event::ToggleIndicator(
                UiIndicator::Kline(KlineIndicator::LiquidityHeatmap),
                vec![binance, bybit, hyperliquid],
            )),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state
                .streams
                .ready_iter()
                .expect("ready streams")
                .filter(|stream| matches!(stream, StreamKind::Depth { .. }))
                .count(),
            0
        );
        let Content::Kline {
            indicators,
            chart: Some(chart),
            ..
        } = &state.content
        else {
            panic!("candlestick chart initialized");
        };
        assert!(!indicators.contains(&KlineIndicator::LiquidityHeatmap));
        assert!(!chart.liquidity_heatmap_runtime_active());
    }

    #[test]
    fn standalone_footprint_history_owns_its_sources_and_aggregation_toggle() {
        let binance = ticker_info(Exchange::BinanceLinear, "BTCUSDT", 0.1);
        let bybit = ticker_info(Exchange::BybitLinear, "BTCUSDT", 0.1);
        let hyperliquid = ticker_info(Exchange::HyperliquidLinear, "BTC", 0.1);
        let mut state = State::default();

        let streams = state.set_content_and_streams(
            vec![binance, bybit, hyperliquid],
            ContentKind::FootprintHistory,
        );
        assert_eq!(streams.len(), 3);
        let Content::FootprintHistory(Some(history)) = &state.content else {
            panic!("standalone Footprint History initialized");
        };
        assert_eq!(history.sources(), &[binance, bybit, hyperliquid]);
        assert!(history.aggregate());

        assert!(matches!(
            state.update(Event::FootprintHistoryAggregationToggled(false)),
            Some(Effect::RefreshStreams)
        ));
        assert_eq!(
            state.streams.ready_iter().expect("ready streams").count(),
            1
        );
    }

    #[test]
    fn regular_panes_do_not_request_frame_rate_ticks() {
        let mut state = State::default();
        assert_eq!(state.tick_subscription_interval_ms(), Some(1000));

        state.content = Content::placeholder(ContentKind::CandlestickChart);
        assert_eq!(state.tick_subscription_interval_ms(), Some(100));
    }
}
