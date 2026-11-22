# Roadmap detail

> **Last reviewed:** 2026-10-11 · **Last updated:** 2026-11-22 · **Change:** minor (G5: co-authoring connected to ShareCraft's browser frame, development only; earlier: G4 checkpoint fidelity proved; user-facing co-authoring remains G5 work; earlier: first live GridCraft command set and in-process co-author adapter complete) · **Target:** Microsoft Excel (Microsoft 365)

Forward-looking plan. The one-page summary is [`ROADMAP.md`](../ROADMAP.md); the ranked work
list is [`gaps.md`](gaps.md); numbers and method are in
[`target-app-parity.md`](target-app-parity.md). Estimates are Opus 5.5 agent-hours.

## Current focus

1. **Review and land the open PR queue** (50 PRs on 2026-10-10): recalc and spill fixes (#148,
   #155, #156, #157), whole-sheet and large-range fixes (#151, #152, #153, #154), chart kinds
   (#158, #161), macOS document open (#164, #167), German UI, numbers and formulas (#163),
   15-digit comparison (#187). 8–15 h. Language work should extend the new `i18n` catalog past the
   ribbon rather than add scattered literals.
2. **XLSX on real files** (gaps #1, #3): corpus run plus Excel opening our output.
3. **Large workbooks and multi-threaded recalc** (gap #2).

## Alpha gate

The core workflows a typical Excel user runs every day, checked end to end on the main platform
(macOS desktop), including saving and reopening the work. Any "no", or a "partial" that blocks the
workflow, would make GridCraft pre-alpha.

| Core workflow | Works end to end? | Evidence | Hours to pass |
|---|---|---|---|
| Build a model: enter data and formulas, recalc, save XLSX, reopen | yes | 492 working functions, dynamic arrays, LET/LAMBDA, names, tables; XLSX round-trip tests (`crates/xlsx/src/tests/roundtrip.rs`). Edge-case bugs (#57 15-digit equality, stale spills #133/#139) give wrong results in specific cases, not in the workflow | 0 (fixes are beta work, gap #6) |
| Open a colleague's XLSX, edit, save, hand it back | partial, not blocking | Typical workbooks open and save with cells, styles, CF, validation, tables, names and pivots; unmodelled parts are dropped with a warning (gap #3); Excel-side repair prompts unverified (gap #1); ~100 MB files fail (#175) | 0 for alpha (gaps #1–#3 are beta gates) |
| Format and present: number formats, styles, conditional formatting, print or PDF | yes | Full number-format language, 47 cell styles, all CF kinds, Page Break Preview, PDF export and printing. PDF can't carry non-Latin text (gap #5) | 0 |
| Analyze: sort, filter, tables, PivotTable | yes | Multi-level sort, AutoFilter, tables with totals, PivotTables with field list, layouts, grouping, refresh and XLSX round-trip. Calculated fields done; slicers missing (gap #8) | 0 |
| Chart the data | partial, not blocking | Column, bar, line, pie, area, scatter, combo, histogram, box, waterfall, funnel, treemap, sunburst and stock charts are created, edited and survive save and reopen; doughnut charts draw every series as a ring (first series innermost) with editable hole size and first slice angle; trendlines are drawn; no axis options (gap #4) | 0 for alpha |
| Import and export CSV | yes | Encoding and delimiter detection on read, CSV/TSV write; exports raw rather than displayed values (gap #19) | 0 |

**Passes:** every core workflow works end to end on macOS with save and reopen; the two partial
rows lose fidelity only on less common content (very large files, unmodelled parts, advanced
chart kinds), which are beta gates, not alpha blockers. Ready for real work ~50% is inside the
40–75% alpha band.

## Beta gates

Beta means: ~75% ready for real work, opens and saves XLSX reliably, remaining gaps are known bugs
and edge features. Each gate links its gap.

| Gate | Gap | Hours |
|---|---|---|
| XLSX proven on a real-world corpus; Excel opens our output without repair | #1 | 15–25 |
| Unmodelled XLSX parts preserved on save | #3 | 10–15 |
| 100 MB workbooks open; 1M rows usable; multi-threaded recalc; 100k-edit recalc < 100 ms | #2 | 25–40 |
| Charts: kinds and options survive round trip; axes, trendlines, error bars | #4 | 25–35 |
| Text in every script renders and exports | #5 | 12–20 |
| Calculation correctness bugs closed; iterative calculation | #6 | 10–15 |
| `.xls` import, password-protected workbooks | #7 | 28–42 |
| PivotTables: slicers, calculated fields, PivotCharts, verified in Excel | #8 | 20–30 |
| Keyboard: key tips, shortcut audit, context menus | #9 | 12–20 |
| Field crashes and freezes closed; autosave/recovery soak | #10 | 8–12 |
| **Total** | | **~165–255 (≈160–240 after overlap with the PR queue)** |

With 4–5 agents and an integrator, about 40–60 wall-clock hours. Needs the owner: running Excel
to check our files, and Windows machines with the reported GPU drivers.

## Milestones

| # | Milestone | Status |
|---|---|---|
| M0 | Foundation: core, number formats, formula language, functions, model, calc, XLSX/CSV, engine | done |
| M1 | Excel look: chrome, ribbon, formula bar, grid, tabs, status bar, control channel | done (polish continues) |
| M2 | Formatting completeness | done |
| M3 | Editing power (AutoComplete, spelling, drag-move/copy) | done |
| M4 | Data (advanced filter, consolidate, what-if) | most; Solver, ToolPak and Power Query remain |
| M5 | Formulas tab (trace arrows, watch window, evaluate, error checking) | done |
| M6 | Conditional formatting manager parity | done |
| M7 | Charts parity | partial; Format pane in; axes, trendlines, error bars, round-trip remain |
| M8 | Insert & Review | partial; ink, icons, comments pane in |
| M9 | Page layout & print | most; Page Layout view remains |
| M10 | PivotTables, Solver, Analysis ToolPak | PivotTables in; slicers, Solver, ToolPak remain |
| M11 | Automation (record actions, scripts) | done |
| M12 | Performance, accessibility, i18n | partial; i18n not started |
| M13 | Release & polish | releases shipping (v0.2.0–v0.4.0, signed) |
| M14 | **Beta gates** (table above) | not started as a milestone; parts in open PRs |
| M15 | Localization infrastructure and the twelve languages | started: ribbon catalog in ja, zh, ko, pt-BR, ru (#31, #104, #82); German in PR #163 |
| M16 | Get & Transform, data model | not started |

## Co-authoring

G3 completes the first fork-side engine slice. `gridcraft/1` has an allow-listed command set with
identity targets for sheets, rows, columns, cells and formula references. The in-process adapter
uses G2's ordered client to apply, roll back and re-run commands, preserves each person's active
sheet and selection, compares entered content across platforms, and records guarded personal
Undo/Redo. G4 adds checkpoints: the agreed state at a ticket's position as deterministic bytes
plus identity session data, loaded with a full recalculation under its entry's stamp. A late
joiner on a checkpoint plus the retained entries, and two fresh editors writing the same
position, produce the same SHA-256 as straight-through clients; desktop and wasm32 agree on
entered content ([coauthor-determinism.md](coauthor-determinism.md#checkpoints-in-g4)). G5 (2026-11)
connects it to ShareCraft's browser frame: sockets, the host bridge, presence, notices and the
screen integration, so people can edit one GridCraft file together in the browser. It is switched
on only in ShareCraft's development run; production stays off until the owner decides. Editing
together needs a live connection, and the editor refuses edits while offline with a message. One
maths library for desktop and browser (desktop sessions wait for it), large content, unshared
commands and Show Changes remain open, as do the I1 and I2 follow-ups before production
(see [gaps.md](gaps.md) §16).

Structural changes on workbooks with at least 100,000 stored cells wait for the server instead of
being shown and re-run locally. Shared sheet insertion and deletion work, but Undo of a sheet add
or delete is refused in a shared session; G3 deliberately does not send a whole sheet in an undo
entry.

The native Windows release-only `heavier_formula_rerun_report` measured this 100,000-cell rule:

| Workload | Clone + apply p50 / p95 | With final full recalculation p50 / p95 |
|---|---:|---:|
| `heavier-content-only` (10 changes) | 242.0 / 299.9 ms | 315.8 / 383.7 ms |
| `heavier-one-structural` (1 row insert) | 129.1 / 143.0 ms | 203.9 / 220.8 ms |

Those heavier-formula figures were not run on wasm, macOS or Linux and are not claims about those
platforms.

## After beta (ranked)

| Work | Gap | Hours |
|---|---|---|
| Localization: catalog past the ribbon, then twelve languages | #11 | 100–165 |
| Get & Transform and the data model | #12 | 60–100 |
| Solver, Analysis ToolPak, Forecast Sheet | #13 | 22–35 |
| Remaining functions, oracle suite | #15 | 18–30 |
| Collaboration and review | #16 | 35–55 |
| Insert objects | #17 | 25–40 |
| Accessibility | #18 | 12–18 |
| Page Layout view, print polish | #14 | 12–20 |
| Mobile layout | #23 | 30–60 |
| AI features with local models | #22 | 30–50 |
| Plugin / custom-function API | #21 | 25–40 |

## Measuring

- `cargo xtask parity` regenerates [`parity-checklist.md`](parity-checklist.md) (ribbon
  catalog vs registered commands); `cargo xtask ci` checks it is current.
- `cargo xtask test`: engine, formula, function, XLSX and UI behaviour (cargo-nextest when
  installed, else `cargo test`), then the doctests.
- `cargo run --release -p gridcraft-ui-egui --example snapshot -- --sample sales out.png`: look at
  the UI offscreen.
- Function coverage: the script method in [function-parity.md](function-parity.md). A
  `cargo xtask functions` that diffs the registry against a checked-in Excel name list would make
  it a CI number (1–2 h).
- No `cargo xtask scorecard` or `bench` yet.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-22 | minor | G5 Task 15: co-authoring connected to ShareCraft's browser frame (two-person acceptance run in the browser, reproducible editor build, offline refusal with a message); development only, production off |
| 2026-11-21 | minor | G5 Task 14: co-authoring proofs run in real browsers (built-in Chromium all pass, headline wasm p95 59.665 ms; Chrome, Edge and Firefox correctness matched, timings missing) |
| 2026-11-19 | minor | G4 checkpoints: agreed-state bytes and identity data, full-recalculation load, checkpoint-plus-suffix, second-writer and native/wasm entered proofs; user-facing work remains G5 |
| 2026-11-19 | minor | First identity-targeted live command set and in-process G2 adapter complete; checkpoints and user-facing G4/G5 integration remain |
| 2026-10-23 | minor | Measuring: tests run through `cargo xtask test` |
| 2026-10-11 | minor | PivotTable calculated fields landed |
| 2026-10-10 | minor | Current focus and M15 updated for the 10-10 merges |
| 2026-10-10 | minor | Added the alpha gate table (six core workflows; passes, stays alpha) |
| 2026-10-10 | major | Created: current focus, beta gates (~160–240 h), milestones moved from ROADMAP.md, post-beta ranking |
