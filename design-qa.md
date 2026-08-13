# Design QA

**Evidence**

- Source visual truth: `target/design-qa/reference-tpo-chart.png` and `target/design-qa/reference-tpo-settings.png`
- Final implementation: `target/design-qa/implementation-tpo-final-native.png` and `target/design-qa/implementation-tpo-final-settings.png`
- Combined comparison: `target/design-qa/comparison-tpo-final.png`
- Viewport: native Windows desktop window at 2560 x 1393 logical pixels
- Pixels and density: source chart 1600 x 897, source settings 2560 x 1440, implementation captures 2560 x 1393, comparison sheet 3200 x 1800. Images were aspect-fitted without cropping or density resampling; the comparison intentionally evaluates component hierarchy and legibility rather than pixel-identical browser chrome.
- State: live BTCUSDT perpetual TPO, one-day profile, 30-minute brackets, user-adjusted and restart-persisted 20 ticks per row, 70% value area, two-bracket initial balance, Auto letter/block display.

**Findings**

- No actionable P0, P1, or P2 findings remain.
- Fonts and typography: TPO letters use Flowsurface's established Azeret Mono chart font with readable row-relative sizing; compact settings text follows the existing native application hierarchy.
- Spacing and layout rhythm: the settings surface remains a compact right-side overlay, uses the application's existing row rhythm, and does not cover the pane header controls. The profile stays anchored to its time cell and expands horizontally as additional brackets accumulate.
- Colors and visual tokens: value-area fading, warning-colored single prints and initial-balance bracket, primary POC emphasis, and native dark-theme tokens remain distinguishable without introducing a separate TradingView-like palette.
- Image quality and asset fidelity: the chart and controls are native vector/text rendering at the captured desktop density; no placeholder imagery or approximated image assets are used.
- Copy and content: labels use standard Market Profile terminology: profile period, letter/block period, ticks per row, session start, value area, initial balance, POC, VAH/VAL, and single prints.

**Open Questions**

- None. The source depicts several mature daily profiles, while the final native capture intentionally shows the just-restarted developing live profile. This is a data-state difference, not a rendering mismatch; historical profile depth is independently configurable and enabled when trade fetching is available.

**Implementation Checklist**

- [x] Native TPO pane and ticker quick action
- [x] Session-aligned, trade-driven TPO aggregation
- [x] Adjustable profile/bracket periods, row ticks, session, value area, initial balance, history, and display mode
- [x] POC, VAH, VAL, initial balance, and single-print rendering
- [x] Live rebuild on setting changes
- [x] Saved-layout persistence across restart
- [x] Full workspace tests, strict Clippy, native build, and live desktop interaction check

**Comparison History**

1. Initial comparison found a P1 readability defect: row text and blocks were sized from one exchange tick instead of the configured TPO row step. Fix: multiply visual row height by `ticks_per_row`. Post-fix evidence: `target/design-qa/implementation-tpo-final-native.png`.
2. The first post-fix single-price state exposed a P1 autoscale defect: one developing row could occupy most of the chart. Fix: derive visible bounds from TPO rows and guarantee a 16-row minimum visible span. Post-fix evidence: `target/design-qa/comparison-tpo-final.png`.
3. Final comparison shows readable letter cells, bounded single-profile autoscaling, POC/VAH/VAL/IB hierarchy, standard settings, and no remaining P0/P1/P2 difference requiring a code change.

**Focused Region Comparison**

- The lower comparison pair isolates the settings surfaces at readable scale. It confirms matching core control vocabulary and shows the adjusted 20-tick value after restart.
- The upper pair isolates the chart profile and confirms row-aligned letters plus POC, VAH, VAL, and initial-balance markings. A tighter crop was unnecessary because these elements remain readable in the 3200 x 1800 combined sheet.

**Follow-up Polish**

- No blocking polish remains. A future optional enhancement could add the reference platform's profile statistics table without changing the TPO calculation model.

final result: passed
