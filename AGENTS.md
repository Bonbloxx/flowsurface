# Agent instructions for this Flowsurface fork

This is a native desktop orderflow workstation. Upstream is
[flowsurface-rs/flowsurface](https://github.com/flowsurface-rs/flowsurface).
This repo (`Bonbloxx/flowsurface`) is a fork that adds surfaces such as Renko,
TPO, Footprint History, Daily Delta, shader heatmaps, and multi-venue
aggregation.

**Default: extend what already exists.** Do not introduce a new GUI toolkit,
web layer, charting library, data pipeline, or crate unless the existing ones
cannot do the job.

---

## Upstream documentation (read this first)

Upstream has no contributor architecture guide. Product and data-flow truth
lives in:

| Source | What to take from it |
| --- | --- |
| [Upstream README](https://github.com/flowsurface-rs/flowsurface/blob/main/README.md) | Surfaces, data sources, GPLv3, iced + Halloy credits |
| [flowsurface.com](https://flowsurface.com/) | Product intent: render liquidity + executions locally, not decorative overlays |
| [Halloy](https://github.com/squidowl/halloy) | Crate split and iced app shape this project copies |
| [iced book — Architecture](https://book.iced.rs/architecture.html) | Elm Architecture: State, Messages, update, view |

### Product constraints from those docs

- Native Rust desktop app. Users download a binary or `cargo run`. No browser,
  no Electron, no remote UI.
- Data is local. Live trades, L2, and klines come from **exchange public
  WebSockets**. Historical klines, open interest, and ticker metadata come from
  **exchange REST**. Optional trade backfill is Settings → Network:
  `Off` | `Exchange` (Binance only) | `Server` (`GET /trades.arrow` Arrow IPC).
- Surfaces that already exist and should be reused before inventing new ones:
  heatmap (historical DOM), candlestick, footprint, time & sales, DOM/ladder,
  comparison. This fork also has Renko, TPO, Footprint History, Daily Delta.
- Layouts persist. Panes can be linked. Themes are user-editable palettes.
- Foundational design is Halloy-shaped iced, not a custom framework.

---

## Architecture you must keep

### Elm / iced

`src/main.rs` runs `iced::daemon` (multi-window). Every screen follows:

1. **State** — structs in the binary crate (`Flowsurface`, `Dashboard`, pane
   `State`, chart types).
2. **Messages** — `Clone` enums (`Flowsurface::Message`, `dashboard::Message`,
   `pane::Message` / `Event`, `chart::Message`).
3. **Update** — `update(&mut self, msg) -> Task<Message>`.
4. **View** — `view(&self) -> Element<Message>` using iced widgets.

Async work (HTTP, WebSocket, Arrow fetch) returns as messages via `Task` /
`Subscription`. Do not spawn ad-hoc UI threads that paint widgets.

### Halloy-style crate split

| Crate | Path | Allowed to contain | Must not contain |
| --- | --- | --- | --- |
| `exchange` | `exchange/` | Venues, REST/WS adapters, `Ticker`/`Trade`/`Kline`/`Depth`, `Price`/`Qty`/`UnixMs` | iced widgets, pane layout, theme drawing |
| `data` (`flowsurface-data`) | `data/` | Serializable layout/config, chart domain, aggregation, `ok_or_default` persistence helpers | `iced::widget`, GPU shaders, windowing |
| `flowsurface` binary | `src/` | iced UI, charts, panels, modals, connector orchestration | New market-data types that belong in `exchange` / `data` |

`data` may depend on `iced_core` for theme `Color`/`Palette` (same as Halloy).
It must not depend on iced widgets.

### Runtime data path

```
exchange adapter (WS/REST)
        ↓ Event / FetchedData
src/connector  (DataSources, stream, fetcher)
        ↓
src/screen/dashboard  (pane grid, UniqueStreams)
        ↓
Content (chart or panel)
        ↓
data domain (PlotData, TimeSeries, TickAggr, studies)
```

Layouts serialize through `data::layout::pane::Pane` into `saved-state.json`
under the OS data dir (override with `FLOWSURFACE_DATA_PATH`).

---

## Reuse-first decision tree

Walk this **in order**. Stop at the first match.

1. **Same axes, extra subplot or overlay** (volume, CVD, OI, daily delta) →
   `KlineIndicator` / `HeatmapIndicator`. Copy `volume.rs` or `daily_delta.rs`.
2. **Different drawing of the same kline/trade series** (candles, footprint,
   Renko, TPO) → `KlineChartKind` on existing `KlineChart`. Do **not** add a
   new `Content` variant.
3. **List / ladder of live book or tape** → `TimeAndSales` or `Ladder` panel.
4. **Percent overlay of several tickers** → `ComparisonChart`.
5. **Historical DOM** → `ShaderHeatmap` (current). Do not extend the legacy
   canvas heatmap unless you are fixing that path.
6. **Same logical market, several venues** → `data::aggregation` catalog
   (`AggregateFeedId`) and/or Footprint History source lists. Do not hard-code
   venue mashups inside a chart.
7. **New exchange** → `exchange/src/adapter/hub/<venue>/` plus `Venue` /
   `Exchange` / `AdapterHandles`. Normalize into existing `Trade`/`Kline`/
   `Depth`.
8. **Historical executed trades** → existing `TradeFetchMode` +
   `src/connector/fetcher.rs`. Do not add a second backfill protocol.
9. **Only then** a new pane `ContentKind` + `Content` + `data::Pane` variant.

Gold example of reuse: `src/chart/indicator/kline/daily_delta.rs` wraps
`FootprintHistoryIndicator` and only draws a new overlay. It does not fetch or
aggregate trades itself.

---

## Workspace map

```
src/main.rs                 iced daemon, top-level Message
src/layout.rs               load/save SavedState ↔ Dashboard
src/style.rs                fonts, Icon glyphs, widget styles, text sizes
src/window.rs               multi-window helpers
src/connector/              DataSources, WS stream resolve, REST/Arrow fetch
src/screen/dashboard.rs     pane_grid, stream fan-in, fetch distribution
src/screen/dashboard/pane.rs  Content, pane chrome, ContentKind switching
src/screen/dashboard/panel/ Time & Sales, Ladder
src/chart.rs                Chart trait, pan/zoom, indicator split rows
src/chart/kline.rs          candles / footprint / Renko / TPO
src/chart/heatmap.rs        legacy canvas heatmap
src/chart/indicator/        subplot + overlay indicators
src/chart/indicator/plot/   BarPlot, LinePlot, indicator_row
src/widget/                 tooltip, toast, multi_split, color_picker, …
src/widget/chart/heatmap/   wgpu shader heatmap (preferred heatmap)
src/modal/                  layout manager, theme, network, pane settings

data/src/layout/pane.rs     persisted Pane tree, ContentKind, Settings
data/src/chart/             Basis, PlotData, KlineChartKind, studies, TPO
data/src/chart/indicator.rs KlineIndicator / HeatmapIndicator enums
data/src/aggregation.rs     multi-venue feed catalog
data/src/stream.rs          PersistStreamKind
data/src/config/            theme, network, sidebar, timezone, saved State
data/src/aggr/              TimeSeries, TickAggr

exchange/src/lib.rs         Ticker, Trade, Kline, Volume, Timeframe
exchange/src/adapter.rs     Venue, Exchange, StreamKind, UniqueStreams
exchange/src/adapter/hub/   binance, bybit, hyperliquid, okex, mexc
exchange/src/unit/          Price, Qty, UnixMs — use these, not raw f32 keys
```

Starter picker (`ContentKind::ALL`) is the user-facing catalog. Internally:

| Picker (`ContentKind`) | Runtime (`Content`) | Domain |
| --- | --- | --- |
| Candlestick / Footprint / Renko / TPO | `Content::Kline` | `KlineChart` + `KlineChartKind` |
| Heatmap Chart | `Content::ShaderHeatmap` | wgpu `HeatmapShader` |
| Heatmap Chart (Legacy) | `Content::Heatmap` | canvas `HeatmapChart` |
| Footprint History | `Content::FootprintHistory` | dedicated pane (also an indicator) |
| Comparison | `Content::Comparison` | `ComparisonChart` |
| Time&Sales / DOM/Ladder | `Content::TimeAndSales` / `Ladder` | panels, not charts |
| Starter Pane | `Content::Starter` | empty picker |

---

## Recipes

### A. Kline subplot indicator (Volume, CVD, OI, Bar Analysis)

Copy `src/chart/indicator/kline/volume.rs` (bars) or `open_interest.rs` /
`cumulative_delta.rs` (lines).

1. Add a variant to `data::chart::indicator::KlineIndicator`.
2. Put it in `FOR_SPOT` and/or `FOR_PERPS`. UI menus read those arrays.
3. Implement `KlineIndicatorImpl` in `src/chart/indicator/kline/<name>.rs`.
4. Build series with `PlotData::map_basis_series` → `BasisSeries` →
   `as_plot_series()`.
5. Draw with `BarPlot` or `LinePlot` via `indicator_row()` — do not write a
   new canvas program.
6. Register in `make_empty()`.
7. Gate with `KlineChartKind::allows_indicator` if it should not appear on
   TPO / candles / etc.
8. If it needs REST/history, implement `fetch_range` and handle
   `FetchedData` the way Open Interest does. Do not open your own HTTP client.

### B. Overlay on the main candle canvas (Daily Delta)

Same enum wiring as A, plus:

- `KlineIndicator::is_overlay() -> true` so it is **not** given a split row.
- Implement `draw_overlay` instead of a real `element` (return an empty row).
- Reuse an existing data pipeline when one already holds the trades
  (`DailyDeltaIndicator` embeds `FootprintHistoryIndicator`).
- Mark `needs_trade_history()` if it uses the shared multi-venue day books.

### C. New kline drawing mode (like Renko or TPO)

Stay on `KlineChart`. Add a `KlineChartKind` variant in
`data/src/chart/kline.rs` (config lives next to it, e.g. `data/src/chart/tpo.rs`).

Then wire:

- `ContentKind` + `ALL` + `Display` in `data/src/layout/pane.rs`
- `Pane` serde variant only if the persisted tree needs new fields; kline
  kinds already persist via `Pane::KlineChart { kind, ... }`
- `Content::new_kline` / `placeholder` / `set_content_and_streams` in
  `src/screen/dashboard/pane.rs`
- Drawing in `src/chart/kline.rs`
- Settings in `src/modal/pane/settings.rs` (`kline_cfg_view` and friends)
- `PaneSetup` defaults (basis, tick multiplier)
- `allows_indicator`, cell-width / scaling limits

Do **not** duplicate pan/zoom, crosshair, axis labels, or `Chart` — those are
shared in `src/chart.rs`.

### D. Truly new pane type

Only when the surface is not a kline kind, heatmap, panel, or comparison.
You must touch, at minimum:

- `data::layout::pane::{Pane, ContentKind, Settings, VisualConfig}`
- `src/screen/dashboard/pane::{Content, State, Event}`
- persist + load in `src/layout.rs` / `data::config::state`
- stream planning (`ResolvedStream`, `PersistStreamKind`)
- settings modal if it has knobs
- `serde(default)` / `ok_or_default` on every new persisted field

New panes inherit config from existing panes of the same type. Preserve that.

### E. New venue / market

Clone `exchange/src/adapter/hub/bybit.rs` (or binance) as the template:

- `hub/<venue>.rs` + `fetch.rs` + `stream.rs`
- `Venue` and `Exchange` variants (`Linear` / `Inverse` / `Spot` as supported)
- `AdapterHandles::spawn_venue`
- `style::Icon` + `venue_icon` if it needs a glyph (Fontello in
  `assets/fonts/`)
- Map every payload into `exchange::{Trade, Kline, Depth, TickerInfo}`
- Use `Price` / `Qty` / `UnixMs` / `TickMultiplier`. Do not key maps on `f32`.

Capabilities differ per venue (`supports_kline_timeframe`,
`supports_heatmap_timeframe`, client vs server depth aggregation). Encode that
on `Exchange`, do not special-case in every chart.

### F. Multi-venue aggregation

- Logical feed identity: `data/src/aggregation.rs` (`AggregateFeedId`,
  `FeedDefinition`, `SourceDefinition`).
- Persist as `PersistStreamKind::AggregateTrades { feed }` or as an explicit
  `Settings.aggregate_sources` list.
- TPO already opts in via `ContentKind::supports_aggregate_feed`. Add new
  consumers there; keep stream planning in the aggregation layer.
- Footprint History / Daily Delta use `Settings.footprint_history_sources` +
  `equivalent_footprint_sources()`.

### G. Historical trades

Do not invent a store. Use `data::TradeFetchMode`:

| Mode | Where | Scope |
| --- | --- | --- |
| `Off` | live WS only | all venues |
| `Exchange` | Binance REST + `data.binance.vision` zips | Binance spot/linear/inverse |
| `Server` | `GET {url}/trades.arrow` | all venues; schema `ts i64`, `price f64`, `qty f64`, `is_sell bool` |

Fetcher: `src/connector/fetcher.rs`. Client: `src/connector/client.rs`
(`ServerClient`). Network UI: `src/modal/network_editor.rs`. Auth tokens go in
the OS keychain, never `saved-state.json`.

Klines, OI, and ticker stats **always** use exchange REST regardless of this
mode.

### H. Settings / chrome

Pane overlays go through `src/modal/pane/` (`stack_modal`, `settings.rs`).
Reuse `labeled_slider`, `classic_slider_row`, `pick_list`, `checkbox`,
`scrollable_content`, `cfg_view_container`. Match existing right-side compact
overlay; do not cover pane header controls.

### I. Color, type, icons

- Colors: `theme.extended_palette()` — `success` / `danger` / `warning` /
  `primary` / `background.*`. Mix with `data::config::theme::{mix_color,
  composite_color, contrast_ratio}`.
- Do not hard-code TradingView-like palettes or one-off RGB for chart ink.
- Type: `style::AZERET_MONO` on charts; `style::text_size::{TINY, SMALL, BODY,
  SECTION, TITLE}`.
- Icons: `style::Icon` + `icon_text`. Add glyphs to `assets/fonts/icons.ttf` /
  `fontello.json` rather than bundling image assets.

Theme persistence is Halloy/iced `Palette` in `data/src/config/theme.rs`.

---

## Persistence rules

Layouts must survive restart and old `saved-state.json` files.

- New struct fields: `#[serde(default)]` and/or `deserialize_with =
  "ok_or_default"` (`data::util::ok_or_default`).
- Keep deprecated stream variants (`PersistStreamKind::DepthAndTrades`) until
  you migrate on load.
- `Ticker` deserializers already accept old packed formats — do not break them.
- Visual knobs live in `VisualConfig` / per-chart `Config`, not ad-hoc maps.
- Network proxy passwords and server bearer tokens are keychain-only.

---

## UI building blocks (use these, do not replace them)

| Need | Use |
| --- | --- |
| App shell / windows | `iced::daemon`, `src/window.rs` |
| Pane tiling | `iced::widget::pane_grid` |
| Chart canvas + pan/zoom/crosshair | `src/chart.rs` `Chart` trait |
| Indicator rows under a chart | `widget::multi_split::MultiSplit` |
| Heatmap cells | `src/widget/chart/heatmap` (wgpu shader) |
| Indicator plots | `BarPlot` / `LinePlot` |
| Toasts | `widget::toast` |
| Link groups | `LinkGroup` + `link_group_button` |
| Drag-reorder indicator list | `widget::column_drag` |
| Audio on trades | existing `src/audio.rs` + `modal/audio.rs` |

iced 0.14 is the GUI. Features already enabled: `wgpu`, `tokio`, `canvas`,
`sipper`, `advanced`, `unconditional-rendering`. `cargo run` uses the `dev`
profile; `--features debug` turns on iced hot-reload (`iced/hot`).

---

## Message and stream flow (where to hook)

User click → `pane::Message` / `Event` → `dashboard::Message` →
`Flowsurface::Message`.

Market data → `exchange::Event` → dashboard distributes into the pane that
owns the `StreamKind`. Fetches complete as
`dashboard::Message::DistributeFetchedData`.

Stream kinds are only `Kline`, `Depth`, `Trades`. If a chart needs trades,
subscribe to `Trades` (and fetch history if `TradeFetchMode` is on). Do not
smuggle trades through kline volume unless the exchange kline is already
directional (`Volume::BuySell`).

`UniqueStreams` de-duplicates subscriptions across panes. Adding a pane should
reuse an existing ticker stream when possible.

---

## Commands

Toolchain is pinned in `rust-toolchain.toml` (currently 1.95.0). rustfmt
`max_width = 100`.

```bash
cargo run                          # debug / day-to-day
cargo run --release                # perf-sensitive heatmap / footprint
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

CI runs the fmt and clippy lines above. Linux CI also installs
`libasound2-dev` (rodio).

There is no web app. Verify UI by launching the native window and exercising
the pane: picker → settings overlay → persist → restart. Cross-check any
layout/indicator you changed on candles, footprint, Renko, and TPO when the
change sits on `KlineChart`.

---

## Do not

- Add egui, GTK, Tauri, a webview, Dioxus, Leptos, or a JS chart (TradingView,
  uPlot, Lightweight Charts, plotters-as-UI).
- Add a database, cache crate, or second HTTP stack. Fetches go through
  `connector` + venue hubs. Optional history is Arrow IPC as specified
  upstream.
- Put iced widgets in `data` or `exchange`.
- Introduce a new `Content` variant for a kline rendering mode.
- Duplicate Footprint History's UTC-day trade book for another indicator.
- Key prices with `f32`/`f64`. Use `exchange::unit::price::Price` /
  `PriceStep`.
- Paint with hardcoded colors that ignore the user palette.
- Change `saved-state.json` shape without `serde` defaults / on-load migration.
- Expand `TradeFetchMode::Exchange` to non-Binance venues; use `Server` or live
  WS instead.
- Refactor crate boundaries, rename `ContentKind` labels, or “clean up” serde
  compatibility as drive-by work.

---

## Closest sibling cheat sheet

| You want | Open first |
| --- | --- |
| Subplot from OHLCV | `src/chart/indicator/kline/volume.rs` |
| Line subplot + REST | `src/chart/indicator/kline/open_interest.rs` |
| Overlay using trade history | `src/chart/indicator/kline/daily_delta.rs` |
| New candle-family drawing | `data/src/chart/kline.rs` `KlineChartKind` + TPO/Renko arms in `src/chart/kline.rs` |
| Pane settings overlay | `src/modal/pane/settings.rs` |
| New venue | `exchange/src/adapter/hub/bybit.rs` |
| Multi-venue feed | `data/src/aggregation.rs` |
| Heatmap feature | `src/widget/chart/heatmap/` (shader), not `src/chart/heatmap.rs` |
| Tape / DOM | `src/screen/dashboard/panel/` |
| Theme token | `data/src/config/theme.rs`, then `style.rs` |

When unsure, grep for the sibling and copy its wiring through `ContentKind` →
`Content` → `data::Pane` → settings → serde. That path is the framework.
)
