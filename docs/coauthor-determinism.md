# Determinism audit for co-authoring

Co-authoring (ShareCraft plan G1, "the proofs") rests on one property: two copies of the engine
that apply the same ordered commands end in the same workbook. This audit lists every source of
nondeterminism in `engine`, `model`, `calc`, `formula`, `functions` and `xlsx` that could reach a
calculated result, a written file or the workbook fingerprint (`engine::fingerprint`). Each entry
is one of:

- **Safe**: it cannot reach those outputs, with the reason.
- **Fixed in G1**: fixed now, with a test and a `fix:` commit.
- **Fixed in G3**: fixed for the first live command set, with the exact regression tests.
- **Open after G3**: outside the first live command set or still requiring a common implementation
  across platforms.

"Shared commands" are the first Stage 2 set in `gridcraft/1`: cell content, number format, font,
fill, borders, alignment, filters, row and column insert and delete, and sheet add, rename and
delete. They carry explicit identity targets before they enter the live log. The in-process G2
adapter preserves each person's view and re-runs pending work from an agreed snapshot. G4 adds
checkpoints (see Checkpoints in G4). This is engine infrastructure only; G5 still has to add the
host bridge and the user-facing collaboration experience.

Audited at branch `coauthor-proofs` after `43d635b0`, November 2026. G3 findings were updated on
branch `coauthor-g3` on 19 November 2026. The G1 long run was re-run at `f92a4178` (see Long run),
after later engine and fingerprint changes.

## Evidence

- `coauthor_replay::two_engines_agree_on_random_sequences` (CI, 500 seeds plus 26 fixtures)
  and `coauthor_replay::long_replay` (ignored, 10,000 seeds in release) apply each sequence to two
  sessions in one process and compare outcomes and both fingerprint digests after every step.
  Engine B's selection and active sheet are moved at random before every step.
- Two sessions in one process use different `std` hash seeds (`RandomState`), so the replay does
  exercise `std` hash order. It does **not** exercise the calculation graph's maps: they use a
  fixed `FxHasher` (`crates/calc/src/recalc/graph.rs`), so both engines get the same order there.
  The hash values are the same on every platform (`write_usize` hashes 8 bytes), but the hash
  table's group width differs between x86_64 (SSE2, 16) and wasm32 (8), so the iteration order of
  the calculation maps can differ between desktop and browser. The wasm comparison (Task 4) is the
  test for that, and this audit found no remaining place where that order reaches a value.
- The long run's result is recorded at the end.

## Fixed in G1

| Source | Where | Fix and test |
| --- | --- | --- |
| RAND had a fixed, unsettable seed. | `calc/src/recalc.rs`, `Calc::rng` | `Calc::set_rand_seed` (commit `3983ca7c`). Test `recalc::tests::rand_seed_sets_the_draws`. The replay seeds each session from the sequence. |
| Overlapping dynamic arrays in one calculation pass: a read of the shared cells took the first (one cell) or last (a range) array in the pass's spill map, in `FxHashMap` order. That order depends on the platform's table layout and on the map's insertion history. | `calc/src/recalc/pass_host.rs`, `cell_value` (`Cover::Many`) and `range_values` (the `ordered` path) | Both now take the anchor first in row-major order, so the read no longer depends on hash order. Which array is laid out is decided separately by write-back: it walks the dirty cells, and when the first anchor is blocked it shows `#SPILL!` there and lays out the other. So a read in the pass can see a value that a later round replaces. Test `recalc::tests::overlapping_spills_in_a_pass_read_the_first_anchor` (failed before the fix: pair 0 read 200, not 100). Commit `fix: overlapping spills in one pass read the first anchor, not hash order`. Before the fix the wrong value was normally replaced in a later round, because the readers of the blocked area are recalculated. But it could feed a dependent's spill size, and a chain of such rounds stops after 8. |

The clock was already settable (`Calc::fixed_now`); the replay sets it from the sequence.

## Fixed in G3

| Source | G3 behaviour and exact tests |
| --- | --- |
| NOW and TODAY | An agreed entry applies its `Stamp.at_ms` to the calculation clock. A locally shown volatile formula asks G2 to re-apply it when the server stamp arrives. Tests: `entry_stamp_controls_now_and_rand`, `volatile_functions_agree_with_fixed_now_and_seed`, and `rollback_rerun_matches_straight_through`. |
| RAND, RANDBETWEEN and RANDARRAY state | `GridSnapshot` copies the calculator, including its RNG cursor, and an entry's `Stamp.seed` starts the agreed stream. Tests: `entry_stamp_controls_now_and_rand`, `snapshot_restores_rand_state`, and `rollback_rerun_matches_straight_through`. |
| Dates typed without a year | A live `cell.set` carries `originYear`; parsing uses that year rather than the receiver's wall clock. Filter criteria (`data.filterBy`, `data.clearFilter`, `data.reapply` re-applying a `>3/4` criterion) read the year of the calculation clock, which the entry stamp sets, and count as a stamp read. Tests: `yearless_date_uses_the_sent_year`, `filter_criteria_read_year_less_dates_in_the_stamped_year`, `filter_criteria_without_year_less_dates_read_no_stamp` and `rollback_rerun_matches_straight_through`. |
| Clock and random state in personal Undo and Redo | A guarded Undo or Redo applies its entry stamp like any other command, so a restored `=NOW()` or `=RAND()` agrees. Tests: `guarded_undo_and_redo_calculate_under_the_entry_stamp`, `pending_guarded_undo_reports_that_it_read_the_stamp` and the Undo and Redo entries in `rollback_rerun_matches_straight_through`. |
| Rows hidden by a filter | Which rows a filter hides follows from the cells' values, often calculated ones (Top 10 and Above Average average and sort floats), so a 1-ULP maths difference can hide a row on one platform only. Those rows are in the `computed` fingerprint stream, not the `entered` one used for room agreement; rows hidden by hand stay entered content. Test: `rows_hidden_by_a_filter_are_not_entered_content`. Cross-platform agreement on filter visibility itself waits for one maths library (or for the originator to send the hidden rows), G4. |
| Personal gates | Edit ranges unlocked with a password and the macro debugger's lock are per-client state. Shared applies ignore personal unlocks (a shared change into a password range is refused everywhere), and the debugger does not start on a shared workbook. Tests: `a_locally_unlocked_edit_range_does_not_split_shared_content` and `the_macro_debugger_cannot_lock_a_shared_workbook`. |
| Commands that read the active sheet, selection or active style | The live wire accepts only explicit targets and explicit resulting values. Filters have idempotent set/remove commands; underline, font-size steps and Format Cells no longer consult the receiver's view in their live form. Tests: `shared_commands_require_explicit_targets`, `filter_commands_ignore_the_receivers_view`, `filter_set_and_remove_are_idempotent`, `underline_with_on_ignores_active_cell`, `font_step_uses_the_sent_size`, `format_cells_validation_uses_each_target_not_active_cell`, and `remote_insert_and_delete_keep_the_local_view`. |
| Per-person history depth | Shared snapshots and agreement fingerprints exclude personal history. Each person keeps at most 100 actions or 128 MiB, counted from content rather than pointer size or sharing, and no peer assumes the same retained depth. Tests: `snapshot_restores_shared_state_not_personal_view`, `undo_is_personal_and_is_a_new_entry`, and `personal_history_obeys_count_and_byte_budgets`. |

The first shared set deliberately leaves non-shared machine-dependent commands open. Shared
sheet insertion and deletion are supported, but undoing a sheet add or delete is refused locally
in a shared session (`sheet_add_undo_is_refused_locally` and
`sheet_delete_undo_is_refused_locally_without_falling_through_history`); G3 does not serialize a
whole sheet into an undo entry.

## Working against local ShareCraft sources

GridCraft's committed lock file pins ShareCraft's `collab-core`, `collab-live` and `protocol`
crates to one ShareCraft git revision. A committed lock file never points at a local path.

When changing GridCraft and ShareCraft together, pass all three local sources to Cargo on the
command line:

```text
cargo build --config 'patch."https://github.com/ThineCraft/ShareCraft".collab-core.path="H:/Source/repos/ThineCraft/ShareCraft/crates/collab-core"' --config 'patch."https://github.com/ThineCraft/ShareCraft".collab-live.path="H:/Source/repos/ThineCraft/ShareCraft/crates/collab-live"' --config 'patch."https://github.com/ThineCraft/ShareCraft".protocol.path="H:/Source/repos/ThineCraft/ShareCraft/crates/protocol"'
```

This override applies only to the invocation. It must not be copied into
`.cargo/config.toml`, and it must not replace the pinned revisions in `Cargo.toml` or
`Cargo.lock`.

## Hash maps and sets

### calc

| Map or set | Where | Class | Why |
| --- | --- | --- | --- |
| `Graph::nodes`, `cell_deps`, `col_deps`, `wide_deps`, `dynamic`, `unsettled_*` (`FxHashMap`/`FxHashSet`) | `recalc/graph.rs` | Safe | Lookups. The iterations (`dependents_of_range` over `cell_deps`/`col_deps`, `settle`, `recalc_all` over `nodes.keys()`, the `dynamic` set) only fill lists that become the dirty set. `run` sorts the dirty set row-major before evaluating, and `spill_changes` is sorted and deduplicated. |
| `seen` in `dirty_closure` | `recalc.rs` | Safe | Returned in hash order, then sorted by `run`. |
| `exprs`, `python`, `results`, `pending`, `in_progress`, `waiting`, `cycle_seen`, `prev`, `members`, `tainted`, `sums`, `used` | `recalc.rs` `run`, `recalc/pass_host.rs` | Safe | Lookups and membership. `write_back` iterates `results`, but each entry writes its own cell and its own `spill_errors` entry, so order cannot matter. `converged` is an `all` over `members`. `iterate_loops` takes its order from the sorted dirty list. |
| `spills` | `recalc/pass_host.rs` | Fixed in G1 (above) | The only place the pass's hash order chose a value. `iterate_loops` removing entries in hash order is order-free. |
| `strategy`, `per_sheet` in `cells_changed` | `recalc.rs` | Safe | Lookups. The seeds they help build go to `dirty_closure`, then are sorted. |
| `index`, `covered` in `loops_among` | `recalc.rs` | Safe | Lookups; the output order comes from `self.circular` and a sorted index. `circular` is only the status-bar list of circular references, and is not in a result or file. |
| `SpillIndex` (`entries`, `blocks`, `sheets`) | `spill_index.rs` | Safe | Lookups. `meeting` returns anchors in hash order, but they become seeds (sorted later) or are applied in `range_values`, where order matters only for overlaps (fixed). |
| `Precision::formats` | `precision.rs` | Safe | Cache of parsed number formats by style. |
| `seen` in `calculate`, `set`/`again` in `iterate_loops`, `wanted` in `range_values` | `recalc/pass_host.rs` | Safe | Membership tests. |

### model

| Map | Where | Class | Why |
| --- | --- | --- | --- |
| `StyleTable::index: HashMap<Style, StyleId>` | `style.rs` | Safe | Lookup only, never iterated; `serde(skip)`. Style ids are given in `intern` call order, which follows the command and the cells it walks (row-major stores), the same in every engine. The fingerprint reads resolved styles, never ids. |

Every other `model` collection that holds content (cells, rows, columns, spills, comments,
hyperlinks) is a `BTreeMap` or a `Vec`.

### formula

| Map | Where | Class | Why |
| --- | --- | --- | --- |
| `Bindings::counts` | `input.rs` | Safe | Membership count for LET/LAMBDA names while normalising input. |

### functions

| Map or set | Where | Class | Why |
| --- | --- | --- | --- |
| `counts`, `seen` (UNIQUE), `seen` (`distinct_keys`), `groups` (GROUPBY), `ids` (subtotal levels), `cell` (PIVOTBY) | `dynamic.rs` | Safe | Lookups. Output order comes from the input rows (first seen), then explicit sorts. |
| `registry` map, test `seen` | `lib.rs` | Safe | Name lookup; test only. |
| `by_char` (TEXTBEFORE/TEXTAFTER delimiters) | `text.rs` | Safe | Lookup of a sorted, deduplicated delimiter list. |
| `index` in `modes` (MODE.MULT) | `stat/descriptive.rs` | Safe | Index into a `Vec` in first-occurrence order. |

### engine

All are `std` maps with a random seed per map, so any order leak on a shared command's path would
already show in the replay.

| Map or set | Where | Class | Why |
| --- | --- | --- | --- |
| Style caches `cache` (`apply_style`), `by_style` (cell fill), `bold` (HTML import), `style_map` (paste between workbooks), `styles` (SYLK), `cache` (pivot output), `pct_styles` (analysis reports) | `cmd/format.rs`, `cmd/edit/cell.rs`, `io.rs`, `cmd/edit/paste_special.rs`, `io/sylk.rs`, `pivot.rs`, `cmd/analysis.rs` | Safe | Memo tables, read by key. Styles are interned while walking ranges and cells in row-major order, so ids come out in the same order everywhere. |
| `measured` (AutoFit column width) | `cmd/format/sizes.rs` | Safe for order | Memo by style and text; the count of measured texts is order-free. The measurement itself is host-dependent (see Other sources). |
| `CfCache::stats`, `RuleStats::counts` | `cf.rs` | Safe | Conditional-format statistics, looked up by key; display only. |
| `counts` in `mode` | `cmd/analysis.rs` | Safe | `max` over values, then the first input value with that count. |
| `col_stats` | `cmd/data/filter.rs` | Safe | Per-criterion thresholds by index. |
| `seen` (Remove Duplicates, Advanced Filter unique, Solver variables, status-bar stats, error tracing `seen`/`drawn`) | `cmd/data/tools.rs`, `cmd/whatif/advanced_filter.rs`, `cmd/solver/model.rs`, `display.rs`, `cmd/formulas.rs` | Safe | `insert` as a first-seen test; output follows the input order. |
| `own`, `local` (name lists) | `cmd/edit/go_to.rs`, `cmd/formulas.rs` | Safe | Membership; output follows `wb.names`. |
| `taken`, `next_suffix` (table and pivot names), `names` (table header sync), `Taken` (macro names) | `cmd/insert/table.rs`, `cmd/insert.rs`, `pivot.rs`, `tables.rs`, `macros/mod.rs` | Safe | Lookups while choosing unique names in column or list order. |
| `keys`, `names` (stash), `blocked` | `cmd/stash.rs`, `macros/mod.rs` | Safe | Membership. |
| `row_label`, `col_label`, `aggs` (Consolidate) | `cmd/whatif/consolidate.rs` | Safe | Lookups; output order comes from ordered key lists, and each aggregate adds in source order. |
| `per` (Scenario Summary) | `cmd/whatif/scenarios.rs` | Safe | Lookups; `m.iter()` builds a list of independent cell writes. |
| `formulas` (SYLK) | `io/sylk.rs` | Safe | Lookup by cell. |
| `anchors` | `cmd.rs` `round_edited_as_displayed` | Safe | Membership. |
| `PicCache` | `cmd/print.rs` | Safe | Picture decode cache for printing. |
| `row_set`, `col_set`, `col_leaves`, `aggs`, `siblings`, `filter_sets`, `cells`, `groups`, `FieldItems::index`, `widths` | `pivot/compute.rs`, `pivot/items.rs`, `pivot/output.rs`, `pivot.rs` | Safe | `row_set`/`col_set` are iterated in hash order into `build_tree`, but every level is then sorted by a total order on distinct items (first occurrence, or class, number and case-folded text, whose keys are distinct). Aggregates are filled in source-row order. `widths` writes one column each, so its order is free. Not a shared command. |
| `bodies`, `by_sheet` (macros), `macro_approvals` | `macros/run.rs`, `lib.rs` | Safe | Lookups. |
| `parsed` number-format cache (global, `Mutex`) | `display.rs` | Safe | Cache by code; clearing it at 4,096 entries changes speed only. |
| `dictionary`, `NAMES` (recent functions) | `cmd/spelling.rs`, `recent.rs` | Safe | Membership tests against fixed lists. |

### xlsx

| Map or set | Where | Class | Why |
| --- | --- | --- | --- |
| Writer interning: `Sst::map`, `NumFmts::map`, `font_map`, `fill_map`, `border_map`, `intern` | `write.rs`, `styles.rs` | Safe | Lookups. Each part's list is a `Vec` in first-use order, which follows sheets and cells in order. |
| `phonetics`, `seen` (twice) | `write.rs` | Safe | Lookup and first-seen tests. |
| `used_names`, `used`, `next` (table names) | `sheet_write.rs`, `sheet_write/parts.rs` | Safe | Unique-name lookups in table order. Cells are written from the row-major store, or from a `BTreeSet` when extra cells merge in. |
| Pivot writer `index`, `taken`, `next_suffix`, `hidden` | `pivot/write.rs` | Safe | Lookups. |
| `PicturesOut::indices: HashMap<*const CellPicture, u32>` | `richdata.rs` | Open after G3 | Iteration order is safe: the map is keyed by address only to share one rich value between cells that share one picture, and the list is in first-use order. But which cells share an `Arc` depends on how the picture got there (copy versus reload), so the same content can give a different number of rich values in the written file. Cell pictures are not shared commands; if they become one, the writer must share rich values by content. |
| `cache` (picture reuse on read) | `richdata.rs` | Safe | Lookup. |
| Passthrough `used`, `written`, `ids`, `NameMap`, `SheetIds`, `reachable`, `by_name`, `dropped_ids`, `gone` | `passthrough/write.rs`, `passthrough/rebase.rs` | Safe | Membership and renaming; kept parts are written in their `Vec` order, filtered by these sets. |
| Passthrough reader `defaults`, `overrides` (content types), `seen`, `local`, `drop` (control shape ids) | `passthrough/read.rs` | Safe | Lookups and membership tests while reading kept parts in the file's own order. |
| Reader maps: `persons`, `num_fmts`, `index`, `SheetNames`, `shared` (shared formulas), `by_id` (threaded comments), `fmts`, `used`, `shared_fmts`, pivot cache `by_id`/`parsed`, controls `by_number`/`rel_by_id`/`props_by_part`/`listed`/`read`/`modelled`, `Package::names` | `read.rs`, `styles.rs`, `sheet_read.rs`, `sheet_read/parts.rs`, `pivot/read.rs`, `controls.rs`, `package.rs` | Safe | Lookups while reading the file in its own order; `Package::names()` sorts (test only). |
| XLS reader `formats`, `shared`, `arrays`, `failed`, `offsets`, `seen` | `xls.rs`, `xls/styles.rs` | Safe | Lookups. |

## Other sources

### Clocks

| Source | Where | Class | Why, and what would make it bite |
| --- | --- | --- | --- |
| NOW and TODAY | `functions/src/date.rs` through `Calc::now` | Fixed in G3 | The entry stamp sets `Calc::fixed_now`; see Fixed in G3. |
| Dates typed without a year (`3/4`, `4-Mar`) | `core/src/parse.rs` `current_year` | Fixed in G3 | Live `cell.set` uses its explicit `originYear`; live filter criteria use the stamped calculation clock's year; ordinary single-user parsing still uses the local year. Slicers still use the local year (not shared). |
| `evaluate` / `evaluate_expr`: system clock and a fixed RNG seed | `calc/src/recalc.rs` | Open after G3 | Used by conditional formats and data validation lists (display), chart series names, sparkline sources, Advanced Filter computed criteria and pivot calculated fields. The last two write cells, so NOW or RAND in them gives different results. No G3 shared command reaches them. A future shared form must pass the session's calculation clock and seed. |
| Conditional-format "today" rules and header and footer dates | `engine/src/cf.rs`, `engine/src/cmd/print/header_footer.rs` | Safe | Display and print only; not in a result, file or fingerprint. |
| Password hash salt (sheet and workbook protection) | `engine/src/cmd/review.rs` `new_password_hash` | Open after G3 | Salt mixes a process counter, the document uid and the clock, and the hash is saved in the file. Protection is not a shared command. If it becomes one, the originator computes the hash and sends it. |
| Random Number Generation tool without a seed | `engine/src/cmd/analysis.rs` | Open after G3 | Seeds from the clock. It is not shared; a future shared form must send the seed or the generated result. |
| Macro time limit | `engine/src/macros/run.rs` | Open after G3 | A macro stops after a wall-clock limit, so a slow client could stop at a different step. Macros are not shared; a future design should share their resulting commands rather than the macro run. |
| Profiling and test timers (`GRIDCRAFT_PROFILE`, `Instant` in tests) | `cmd.rs`, `recalc.rs`, tests | Safe | Timing output only. |

### Random numbers

| Source | Class | Why |
| --- | --- | --- |
| RAND, RANDBETWEEN, RANDARRAY (`PassHost::random`) | Fixed in G3 | `GridSnapshot` includes `Calc`, so rollback restores the RNG cursor; the agreed entry seed starts the same stream. See Fixed in G3. |
| `std` `RandomState` hash seeds | Safe | Covered by the replay: two engines in one process have different seeds. |

### Pointers and addresses

| Source | Class | Why |
| --- | --- | --- |
| `Arc::ptr_eq` shortcuts (unchanged sheets, bands and workbooks) | Safe | A pointer match means the content is the same; a miss falls back to comparing content. |
| `PicturesOut` keyed by `*const CellPicture` | Open after G3 | See xlsx above: the order is safe, but `Arc` sharing can change how many rich values are written. Pictures are not shared commands. |
| Undo history bound (`CellStore::retained_bytes_against`) | Fixed in G3 for shared history | The existing single-user history remains platform-dependent. The G3 personal history instead uses content-based accounting, stays outside snapshots and fingerprints, and makes no cross-client depth assumption. |
| Sheet, document and recording uids (`NEXT_SHEET_UID`, `NEXT_UID`, `NEXT_RECORDING`) | Safe | Process counters; never written to a file or the fingerprint. G3 gives sheets shared identities of its own. |

### Floating point and formatting

| Source | Class | Why |
| --- | --- | --- |
| Rust float formatting (`{}`, `{:.*}`, `{:e}`) | Safe | Exact and platform-independent. The fingerprint hashes IEEE-754 bits, not text. |
| Basic arithmetic, `floor`, `round`, `ceil`, `sqrt` | Safe | Correctly rounded IEEE-754 on every target. |
| Transcendental functions (`exp`, `ln`, `log10`, `powf`, `sin`, …) in `functions` (distributions, financial, engineering, math) and `powi` in `numfmt` | Open after G3 | They come from the platform's maths library on the desktop and from Rust's own on wasm32, and may differ in the last bit. `powi` is documented as unspecified precision. A last-bit difference reaches computed values and, through `core::number_to_text` (`log10().floor()`), possibly the shown text of a number, which formulas such as `&` and Remove Duplicates read. Task 4's maths grid (x86_64 Windows MSVC against wasm32 under Node): entered content always agrees. 64 of 1,584 maths cells differ by 1 ULP. In a default-width cell they look the same, but at 15-digit text (`&`, CONCAT, the formula bar) 3 of them differ: `SIN(pi/4)`, `POWER(2,-0.5)` and `1e5^0.37` (6 texts, with the 15-digit scientific TEXT format; `allowed-text-differences.json`). G3 therefore uses only the entered-content digest for cross-platform agreement. A common maths library remains required before desktop and browser computed values can be treated as identical. |
| Row auto-fit after formatting | Safe | `auto_row_height` uses `approx_text_width` arithmetic, not the host's font measurement, so the row heights in the fingerprint are the same everywhere. |
| AutoFit Column Width | Open after G3 | Uses the host's `Session::text_width` (font metrics) when set, so desktop and browser give different widths. It is not a shared command; a future shared form must send the resulting widths. |

### Threads and parallel iteration

None in these crates: no `rayon`, `thread::spawn` or parallel iterators. Global state is behind
`OnceLock` or a `Mutex` cache (number formats, function registry, dictionary) and holds no
per-workbook results.

### Calculation history

| Source | Class | Why |
| --- | --- | --- |
| Incremental versus full recalculation | Safe for normal calculation | `command_sequence_fuzz` checks that a full recalculation from scratch gives the incremental values. G3's rollback rebuilds calculation, so this is the property it needs. |
| Iterative calculation and manual mode | Open after G3 | Iterative results depend on which formulas were dirty and on the evaluation order (Gauss-Seidel); manual mode leaves values stale. Both differ from a full recalculation by design (`command_sequence_fuzz` skips them). Calculation options are not shared commands. A future shared design must either share them with a full recalculation on every client or keep them per workbook and recalculate in full after each agreed step. |
| More than 8 spill rounds in one recalculation | Open after G3 | `run` stops after 8 rounds. A pathological chain of spills could stop at different points after an incremental edit and after a rebuild. Not seen in the replay. |

## Checkpoints in G4

A checkpoint is the XLSX of the agreed state at a ticket's exact position, with the identity
tables as session data. Branch `coauthor-g4`, November 2026.

| Property | G4 behaviour and exact tests |
| --- | --- |
| Agreed state only | A write or verify ticket writes the agreed snapshot at its position; a change still waiting is shown but never written. Tests: `grid_checkpoint_uses_the_agreed_snapshot` and, with a waiting change during the ticket, `checkpoint_then_catchup_equals_straight_through`. |
| Deterministic bytes and identities | Personal view (active sheet, selection, scroll, zoom, tab and scroll-bar settings) is replaced by the stored view before writing, so two people in the same agreed state write the same bytes, identity data and SHA-256. Tests: `grid_checkpoint_bytes_are_deterministic` and `grid_checkpoint_restores_stored_view_before_writing`. |
| Identity data | The session data carries every sheet, row and column identity, including who deleted each one, and is validated as a whole before an editor sees it. Tests: `checkpoint_session_data_restores_deletion_ownership` and `workbook_ids_reject_inconsistent_session_data`. |
| Full recalculation on load | A checkpoint loads with a full recalculation under the stamp of the entry at its position (clock and RAND seed), not with the file's cached values. Tests: `checkpoint_load_recalculates_every_formula` and `checkpoint_load_stamps_the_first_iterative_calculation`. |
| Row heights | File › Open's auto-fit of imported rows is not applied a second time on a checkpoint load, so the row-height loss found in G3 (8 of 10,000 generated cases) is fixed; ordinary File › Open still fits imported rows. The load's recalculation fits no formula rows either: a peer's heights are not always the fitted ones (a file base keeps a taller stored height, and ranges over 100,000 rows are not fitted), so a joiner keeps exactly the checkpoint's heights. Live edits still fit the rows of recalculated formulas, alike on every peer. Tests: `checkpoint_reload_preserves_wrapped_formula_row_height`, `checkpoint_reload_preserves_seed_3996_row_heights`, `ordinary_file_open_still_fits_imported_rows`, `coauthor_checkpoint_load_keeps_peers_formula_row_heights`, `coauthor_checkpoint_joiner_fingerprint_matches_peers_after_more_edits`, and `coauthor_save_reload::save_reload_lossless` with its checkpoint cases. |
| Checkpoint plus suffix | A late joiner that loads a checkpoint written while a change waited, with its identity data, and replays the retained entries agrees in entered content with clients that applied everything, after every later entry. Its head checkpoint has the same bytes, identity data and SHA-256 as theirs. The run includes NOW, RAND, row and column insert and delete, a sheet add and rename, and an Undo of a row deletion made before the checkpoint. Test: `checkpoint_then_catchup_equals_straight_through`. Loading the same bytes as a plain file, without the identity data, makes that Undo do nothing on the late joiner and the test fails. |
| Second writer | Two fresh editors that load the same base and identity data and replay the same entries write identical bytes, identity data and SHA-256 at the head, equal to the straight-through writers'. The identity data is identical too for the straight-through writer, the late joiner and the base's writer, and a verify answer carries its SHA-256 (`session_sha256`), which the room compares with the writer's upload. Test: `checkpoint_hash_matches_between_two_grid_editors`. |
| Across platforms | Room agreement compares the entered digest only. The same checkpoint run under wasm32 (Node, wasm-bindgen test runner) gives the native entered digests at the base and the head. Test: `wasm_replay::native_and_wasm_checkpoint_agree_on_entered_content`. In this run the head's SHA-256 also matched the native one, but that is reported, not asserted: checkpoint bytes carry computed values, and the maths-library differences above can change them. |

Still open after G4: one maths library for desktop and browser (until then a browser writer and a
desktop verifier can disagree on bytes for the same entered content); runs in real browsers rather
than Node; the G5 host bridge; large content (C20); and commands outside the shared set.

## Long run

`cargo test --release --locked --offline -p gridcraft-engine --test it coauthor_replay::long_replay -- --ignored --nocapture`,
seeds 100000 to 109999, 60 random steps each, on x86_64 Windows, at `f92a4178`:

- Passed in 311.1 s: 10,000 sequences, 625,130 steps, 0.0 % errors, 18.5 % structural.
- The `entered` digest changed on 463,024 of 509,684 non-structural steps (90.8 %); the
  `computed` digest changed on 443,516 of 625,130 steps (70.9 %).
- No seed failed. The two engines agreed on outcome and both digests after every step, with
  engine B's view moved before each step.
- Scope: identical command histories from a new workbook, in one process on x86_64 Windows. This
  G1 run did not drive filters or prove rollback against straight application. G3 adds focused
  explicit-filter tests and `rollback_rerun_matches_straight_through`, including snapshot RNG
  state. G4's checkpoint tests are listed under Checkpoints in G4; the G1 run proves nothing
  across platforms.
- A run at `d0f5983b` (248.4 s), before the row-height writer fix (`723f2f70`) and the fingerprint's
  blank-cell rule (`93951043`), gave exactly the same counts.
- An earlier run at `965571a1` (241.8 s, the same counts except 463,022 `entered` changes)
  predates later changes to a shared command (`home.borders` re-fits rows, `4b362647`) and to
  what the fingerprint compares (`6f612608`, `dba599ee`); it is superseded by the runs above.
