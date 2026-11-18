# Shared commands that still read the selection or the active sheet

Input for co-authoring plan G3 (making the shared commands target-explicit). The two-engine replay
(`crates/engine/tests/it/coauthor_replay.rs`) generates only commands whose result depends on
their parameters and the workbook, never on the selection, the active cell or the active sheet,
which are each person's own view. Found by reading each command's code on branch
`coauthor-proofs` (from `sharecraft` 017d0b9d).

## Left out of generation

This still reads the selection or active sheet by design and has no shared form.

| Command | What it reads | Where |
| --- | --- | --- |
| `data.filter` | A toggle. On the active sheet it looks for a table at the active cell (`selection.active`), not at `range`, before deciding what to toggle. Whether it adds or removes the filter depends on the current state, so two people sending it at once cancel out. | `cmd/data/filter.rs`, `toggle_filter` |
The toggle remains outside generated and live sequences. Shared filtering uses the idempotent,
target-explicit commands below instead.

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
| `home.underline`, `home.doubleUnderline` | `sheet`, `range`, `on`, `style` | No `on`: toggles from the active cell's underline. The explicit form does not read it. |
| `home.increaseFontSize`, `home.decreaseFontSize` | `sheet`, `range`, `size` | No `size`: the local UI form steps from the active cell. The explicit form applies the sent half-point size. |
| `home.formatCells` | `sheet`, `range`, `style` | The explicit form validates the patch against every distinct target style before changing any cell. |
| `data.filterSet`, `data.filterRemove` | `sheet`, `range` | Idempotently install or remove exactly that AutoFilter. Another range is an error. |
| `data.filterBy` | `sheet`, `range`, `column`, and one criterion | The named AutoFilter must already exist; no selection-based filter is installed. |
| `data.clearFilter`, `data.reapply` | `sheet`, `range` | Clear criteria or reapply exactly the named AutoFilter. |
| `home.insertRows`, `home.deleteRows`, `home.insertColumns`, `home.deleteColumns` | `sheet`, `rows` or `cols` | The selection's rows or columns. |
| `home.insertSheet` | `name`, `before` | No `before`: after the active sheet. No `name`: the next free `SheetN`. It also makes the new sheet active (view state only). |
| `sheet.rename`, `home.deleteSheet` | `sheet` (by name) | The active sheet. |

## Other notes for G3

- Two shared commands change the view of whoever applies them, even with explicit targets
  (`explicit_targets_leave_the_active_sheet_alone` pins this): `home.insertSheet` makes the new
  sheet active, and `home.deleteSheet` resets the selection to A1, even when another sheet is
  deleted. Neither changes the workbook differently, but a remote change must not move a
  co-author's view, so G3's apply loop has to keep the view when it applies others' commands.

- Typed dates without a year (`3/4`, `4-Mar`) take the current year from the system clock
  (`gridcraft_core::parse::current_year`), not from the calculation's fixed clock. Two engines on
  either side of New Year would store different dates. The generator does not type such dates.
- Formatting a range re-fits the row heights from font metrics (`fit_rows`). Row heights are part
  of the fingerprint, so this matters for the desktop and browser comparison (Task 4).
