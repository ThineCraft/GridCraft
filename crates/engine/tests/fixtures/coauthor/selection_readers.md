# Shared commands that still read the selection or the active sheet

Input for co-authoring plan G3 (making the shared commands target-explicit). The two-engine replay
(`crates/engine/tests/it/coauthor_replay.rs`) generates only commands whose result depends on
their parameters and the workbook, never on the selection, the active cell or the active sheet,
which are each person's own view. Found by reading each command's code on branch
`coauthor-proofs` (from `sharecraft` 017d0b9d).

## Left out of generation

These read the selection or the active sheet even when given explicit parameters.

| Command | What it reads | Where |
| --- | --- | --- |
| `data.filter` | A toggle. On the active sheet it looks for a table at the active cell (`selection.active`), not at `range`, before deciding what to toggle. Whether it adds or removes the filter depends on the current state, so two people sending it at once cancel out. | `cmd/data/filter.rs`, `toggle_filter` |
| `data.filterBy` | Has no `sheet` parameter: always filters the active sheet. With no filter on that sheet it first installs one over the current region of the selection. | `cmd/data/filter.rs`, `filter_by` |
| `data.clearFilter` | Has no `sheet` parameter: always clears the active sheet's criteria. | `cmd/data/filter.rs`, `clear_filter` |
| `data.reapply` | Same: active sheet only. | `cmd/data/filter.rs`, `reapply` |
| `home.underline`, `home.doubleUnderline` | Reads the active cell's underline (it is computed even when `on` is given, though then unused); without `on` it toggles from the active cell. | `cmd/format.rs`, `underline` |
| `home.increaseFontSize`, `home.decreaseFontSize` | Steps from the active cell's font size, whatever `range` says. | `cmd/format.rs`, `step_font` |
| `home.formatCells` | Validates the style patch by merging it into the active cell's style. The result applied to `range` does not depend on it, but a patch could be accepted or refused depending on the active cell. | `cmd/format.rs`, `format_cells` |

Set and clear filter are therefore not in the generated sequences. G3 needs a filter command
with an explicit sheet, range and criteria (set and clear), with no toggle.

## Generated only in their explicit form

These read the selection or the active sheet only when a parameter is missing. The generator
always gives that parameter.

| Command | Explicit form used | Without it |
| --- | --- | --- |
| `cell.set` | `sheet`, `cell`, `input`; never `array` | No `cell`: the active cell. `array: true`: the selection, when it holds the cell on the active sheet. No `sheet`: the active sheet. |
| `edit.clearContents` | `sheet`, `range` | The selection's areas on the active sheet. |
| `home.numberFormat`, `home.fontName`, `home.fontSize`, `home.fontColor`, `home.fillColor`, `home.borders`, `home.alignTop`, `home.alignMiddle`, `home.alignBottom` | `sheet`, `range` | The selection's areas on the active sheet. A `Sheet!` prefix on `range` is honoured by the sheet lookup, but the range parser drops it, so the generator always passes `sheet`. |
| `home.bold`, `home.italic`, `home.wrapText` | `sheet`, `range`, `on` | No `on`: toggles from the active cell's style. |
| `home.alignLeft`, `home.alignCenter`, `home.alignRight` | `sheet`, `range`, `on: true` | No `on`: if the active cell already has that alignment, sets General instead. |
| `home.insertRows`, `home.deleteRows`, `home.insertColumns`, `home.deleteColumns` | `sheet`, `rows` or `cols` | The selection's rows or columns. |
| `home.insertSheet` | `name`, `before` | No `before`: after the active sheet. No `name`: the next free `SheetN`. It also makes the new sheet active (view state only). |
| `sheet.rename`, `home.deleteSheet` | `sheet` (by name) | The active sheet. |

## Other notes for G3

- Typed dates without a year (`3/4`, `4-Mar`) take the current year from the system clock
  (`gridcraft_core::parse::current_year`), not from the calculation's fixed clock. Two engines on
  either side of New Year would store different dates. The generator does not type such dates.
- Formatting a range re-fits the row heights from font metrics (`fit_rows`). Row heights are part
  of the fingerprint, so this matters for the desktop and browser comparison (Task 4).
