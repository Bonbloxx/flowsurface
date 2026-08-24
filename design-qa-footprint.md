# Footprint Design QA

**Evidence**

- Source visual truth: `C:\Users\verne\AppData\Local\Temp\codex-clipboard-7cbb74ba-1c61-47d2-9ae4-3b695457dc9f.png`
- Volume-profile visual truth: `C:\Users\verne\AppData\Local\Temp\codex-clipboard-3117de0c-86fa-45de-9459-806dfb038cd5.png`
- User-confirmed intermediate issue: `C:\Users\verne\AppData\Local\Temp\codex-clipboard-b2025edc-b1b2-42a2-8fbd-6235be4a1421.png`
- Final native implementation: `target/codex-footprint-qa-after-candle-both-sides.png`
- Full-view comparison: `target/codex-footprint-qa-comparison-final.png`
- Focused comparison: `target/codex-footprint-qa-focus-final.png`
- Viewport: isolated native Windows window at 1528 x 739 pixels; chart content was 1382 x 625 pixels. The source is a 1512 x 598 chart-only image.
- Density normalization: both captures were reviewed at device scale 1. The full comparison keeps original pixels; the focused comparison aspect-fits representative candle groups to equal 400 x 560 regions for structural inspection.
- State: source-market data is unavailable, so values and price action differ. The implementation uses live BTCUSDT perpetual aggregate trades, M1, 500x price grouping, Bid x Ask clusters, and the user's existing theme. QA therefore compares the requested footprint geometry rather than candle-for-candle data.

**Findings**

- No actionable P0, P1, or P2 findings remain for the requested footprint layout.
- Fonts and typography: the established Azeret Mono chart font remains in use. Bid and ask values are readable at the 104 px reference cadence and now have 3 screen pixels of padding on each side of the shared midpoint.
- Spacing and layout rhythm: each column uses a roughly 10 px candle lane with two extra screen pixels before the candle and a minimum 7 px candle-to-ladder gutter after it, followed by two symmetric histogram halves. The fills meet at one exact midpoint with no bar gap, while the labels retain 6 px total visual separation.
- Colors and visual tokens: the user's existing success, danger, primary, text, and background theme colors are unchanged. Labeled histograms retain their original 0.36 opacity; the improved visual weight comes from bar geometry, not brighter colors. A 0.75 screen-pixel black outline separates each nonzero tick-level bar.
- Volume hierarchy: Bid x Ask widths use a monotonic square-root profile within each candle. At a 42 px half-lane with a 33M maximum, 3M occupies about 13 px, 15M about 28 px, 25M about 37 px, and 33M the full 42 px. This makes low, medium, and high-volume rows visually decisive without changing their ordering or overstating the candle maximum.
- Image quality and asset fidelity: candles, values, fills, and POC outlines remain native vector/text rendering. No raster assets or approximations were introduced.
- Copy and content: all displayed bid, ask, ticker, timeframe, and multiplier values remain live application data.

**Comparison History**

1. The original implementation had an 8 screen-pixel empty gutter between the bid and ask histograms, width-dependent multi-pixel wicks, subdued 0.36-alpha fills, and no Bid x Ask POC row outline. Fix: make the histogram midpoint contiguous, keep the wick at one screen pixel, raise labeled-fill alpha to 0.72, retain a two-screen-pixel minimum nonzero fill, and add the existing primary-token POC outline.
2. The first revised native capture showed the requested structure, but the two text anchors visually touched and the 120 px cadence was wider than the reference. The user confirmed the structure was much better and requested text spacing. Fix: add 3 screen pixels of padding per label, without moving the bars, and set the default Bid x Ask cadence to 104 px. Post-fix evidence is the final native and combined comparison images above.
3. The next review found that long ask values and the POC outline could visually touch the candle body. Fix: reserve a zoom-stable minimum 5 screen-pixel candle-to-ladder gutter. The final native capture confirms separation without reopening the bid/ask midpoint.
4. A focused crop showed the POC outline's vertical edges cutting into the first and last label glyphs. Fix: expand the outline by 3 screen pixels on both horizontal sides while retaining at least 2 screen pixels between the expanded outline and candle. The final capture shows the full labels unobstructed.
5. The linear Bid x Ask profile made both 3M rows and high-volume rows too narrow to read from bar shape alone when a candle contained a much larger maximum. Fix: replace linear width mapping with a monotonic square-root curve. The final capture shows broad high-volume blocks, clearly present mid-volume rows, and tapered low-volume rows while the maximum remains lane-bounded.
6. The prior visibility pass had increased labeled-bar opacity from 0.36 to 0.72, making the existing red/green theme hues appear more saturated than the app's original footprint. Fix: restore 0.36 opacity and add a thin black outline around every nonzero bid/ask bar so adjacent price levels remain distinct without changing the palette.
7. The candle still sat slightly close to the surrounding footprint geometry. Fix: add two zoom-stable screen pixels before the candle and two after it, increasing the current-ladder gap to at least 7 screen pixels while preserving the contiguous bid/ask midpoint.

**Focused Region Comparison**

- The focused sheet compares three reference footprints against representative final native footprints. It verifies a left candle lane, visible candle-to-ladder gutter, hairline wick, adjacent bid/ask fills, separated labels, strong histogram presence, and full-row POC outline.
- Different live market states produce different vertical row counts; this is expected and remains freely adjustable with the chart's uncapped pan and expanded zoom controls.

**Implementation Checklist**

- [x] One-screen-pixel candle wick across supported zoom levels
- [x] Reference-width 104 px Bid x Ask cadence
- [x] Zero geometric gap between histogram halves
- [x] Six-screen-pixel total text separation at the midpoint
- [x] Extra two-screen-pixel separation before and after the candle
- [x] Minimum seven-screen-pixel candle-to-ladder separation
- [x] Stronger bid/ask fills with current theme colors
- [x] Readable low-, medium-, and high-volume bar hierarchy
- [x] Thin black separation around each nonzero tick-level bar
- [x] Original labeled-bar opacity and theme colors restored
- [x] Bid x Ask POC row outline
- [x] Three-screen-pixel label clearance inside the POC outline
- [x] Focused geometry, zoom, workspace, Clippy, release-build, and native-capture checks

**Follow-up Polish**

- None required for the requested layout.

final result: passed
