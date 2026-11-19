use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use collab_live::ids::{LiveId, Lookup};
use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, DefinedName, Formula, LineInfo, Sheet, Style, StyleId, Workbook};
use protocol::live::MAX_PAYLOAD_BYTES;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::command::{StylePatch, encode_formula_in_workbook, resolve_cell_input};
use super::{CellId, CoauthorError, IdentityPosition, IdentityReference, SharedCommand, TargetGone, WorkbookIds};
use crate::{EngineError, Session};

pub const PERSONAL_HISTORY_ACTIONS: usize = 100;
pub const PERSONAL_HISTORY_BYTES: usize = 128 * 1024 * 1024;
/// Every guard needs substantially more than 64 wire bytes. Deriving the limit from the live
/// payload ceiling keeps validation bounded even if a future compact representation gets close
/// to that floor.
const MAX_GUARDS_PER_ENTRY: usize = MAX_PAYLOAD_BYTES / 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedValue<T> {
    pub target: T,
    pub expected: GuardedCellInput,
    pub restore: GuardedCellInput,
}

pub type GuardedCellContent = GuardedValue<CellId>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedCellInput {
    pub input: String,
    pub formula: bool,
    pub references: Vec<IdentityReference>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum StyleTarget {
    Cell { cell: CellId },
    Row { sheet: LiveId, row: LiveId },
    Column { sheet: LiveId, col: LiveId },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedStyleLeaf {
    pub target: StyleTarget,
    pub path: Vec<String>,
    pub expected: StylePatch,
    pub restore: StylePatch,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedLineState {
    pub target: StyleTarget,
    pub expected: Option<LineInfo>,
    pub restore: Option<LineInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedAxisState {
    pub sheet: LiveId,
    pub before: IdentityPosition,
    pub affected: Vec<LiveId>,
    pub fillers: Vec<LiveId>,
    pub expected_present: bool,
    pub entered: [u8; 32],
    pub filler_entered: [u8; 32],
    pub cells: Vec<GuardedCellContent>,
    pub styles: Vec<GuardedStyleLeaf>,
    pub lines: Vec<GuardedLineState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedSheetState {
    pub sheet: LiveId,
    pub at: u32,
    pub entered: [u8; 32],
    pub rows: collab_live::ids::OrderedIds,
    pub cols: collab_live::ids::OrderedIds,
    pub snapshot: GuardedSheetSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedSheetFormula {
    pub target: CellId,
    pub input: GuardedCellInput,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GuardedSheetSnapshot {
    sheet: Sheet,
    file_id: u32,
    formulas: Vec<GuardedSheetFormula>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardedSheetSnapshotWire {
    version: u8,
    sheet: Sheet,
    file_id: u32,
    formulas: Vec<GuardedSheetFormula>,
}

impl Serialize for GuardedSheetSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let encoded = serde_json::to_string(&GuardedSheetSnapshotWire {
            version: 1,
            sheet: self.sheet.clone(),
            file_id: self.file_id,
            formulas: self.formulas.clone(),
        })
        .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for GuardedSheetSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let wire: GuardedSheetSnapshotWire = serde_json::from_str(&encoded).map_err(serde::de::Error::custom)?;
        if wire.version != 1 {
            return Err(serde::de::Error::custom("unsupported guarded sheet snapshot version"));
        }
        Ok(Self { sheet: wire.sheet, file_id: wire.file_id, formulas: wire.formulas })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedWholeState {
    pub sheets: Vec<GuardedSheetState>,
    pub names: Vec<DefinedName>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum GuardedStructural {
    InsertRows {
        state: GuardedAxisState,
    },
    DeleteRows {
        state: GuardedAxisState,
    },
    InsertColumns {
        state: GuardedAxisState,
    },
    DeleteColumns {
        state: GuardedAxisState,
    },
    AddSheet {
        sheet: LiveId,
        before: IdentityPosition,
        expected_present: bool,
        entered: [u8; 32],
        snapshot: Box<GuardedSheetSnapshot>,
    },
    RenameSheet {
        sheet: LiveId,
        expected: String,
        restore: String,
    },
    WholeAction {
        label: String,
        sheet: LiveId,
        before: IdentityPosition,
        expected_after: bool,
        before_state: Box<GuardedWholeState>,
        after_state: Box<GuardedWholeState>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum GuardedPatch {
    Cells(Vec<GuardedCellContent>),
    Styles(Vec<GuardedStyleLeaf>),
    Composite(Vec<GuardedPatch>),
    Structural(Box<GuardedStructural>),
}

impl GuardedPatch {
    fn reversed(&self) -> Self {
        match self {
            Self::Cells(values) => Self::Cells(
                values
                    .iter()
                    .map(|value| GuardedValue { target: value.target, expected: value.restore.clone(), restore: value.expected.clone() })
                    .collect(),
            ),
            Self::Styles(values) => Self::Styles(
                values
                    .iter()
                    .map(|value| GuardedStyleLeaf {
                        target: value.target,
                        path: value.path.clone(),
                        expected: value.restore.clone(),
                        restore: value.expected.clone(),
                    })
                    .collect(),
            ),
            Self::Composite(values) => Self::Composite(values.iter().map(Self::reversed).collect()),
            Self::Structural(structural) => Self::Structural(Box::new(structural.reversed())),
        }
    }

    fn field_count(&self) -> usize {
        match self {
            Self::Cells(values) => values.len(),
            Self::Styles(values) => values.len(),
            Self::Composite(values) => values.iter().fold(0usize, |count, value| count.saturating_add(value.field_count())),
            Self::Structural(_) => 1,
        }
    }
}

impl GuardedStructural {
    fn reversed(&self) -> Self {
        match self {
            Self::InsertRows { state } => Self::InsertRows { state: reverse_axis(state) },
            Self::DeleteRows { state } => Self::DeleteRows { state: reverse_axis(state) },
            Self::InsertColumns { state } => Self::InsertColumns { state: reverse_axis(state) },
            Self::DeleteColumns { state } => Self::DeleteColumns { state: reverse_axis(state) },
            Self::AddSheet { sheet, before, expected_present, entered, snapshot } => {
                Self::AddSheet { sheet: *sheet, before: *before, expected_present: !expected_present, entered: *entered, snapshot: snapshot.clone() }
            }
            Self::RenameSheet { sheet, expected, restore } => {
                Self::RenameSheet { sheet: *sheet, expected: restore.clone(), restore: expected.clone() }
            }
            Self::WholeAction { label, sheet, before, expected_after, before_state, after_state } => Self::WholeAction {
                label: label.clone(),
                sheet: *sheet,
                before: *before,
                expected_after: !expected_after,
                before_state: before_state.clone(),
                after_state: after_state.clone(),
            },
        }
    }
}

fn reverse_axis(state: &GuardedAxisState) -> GuardedAxisState {
    GuardedAxisState {
        sheet: state.sheet,
        before: state.before,
        affected: state.affected.clone(),
        fillers: state.fillers.clone(),
        expected_present: !state.expected_present,
        entered: state.entered,
        filler_entered: state.filler_entered,
        cells: state
            .cells
            .iter()
            .map(|value| GuardedValue { target: value.target, expected: value.restore.clone(), restore: value.expected.clone() })
            .collect(),
        styles: state
            .styles
            .iter()
            .map(|value| GuardedStyleLeaf {
                target: value.target,
                path: value.path.clone(),
                expected: value.restore.clone(),
                restore: value.expected.clone(),
            })
            .collect(),
        lines: state.lines.iter().map(|value| GuardedLineState { target: value.target, expected: value.restore, restore: value.expected }).collect(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UndoAction {
    pub action: u64,
    pub label: String,
    pub undo: GuardedPatch,
    pub redo: GuardedPatch,
    pub retained_bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoReport {
    pub restored: usize,
    pub left: usize,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum HistoryError {
    #[error("There is nothing to undo while sharing")]
    NothingToUndo,
    #[error("There is nothing to redo while sharing")]
    NothingToRedo,
    #[error("Wait for the current shared Undo or Redo to finish")]
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryDirection {
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingMove {
    direction: HistoryDirection,
    source_action: u64,
}

#[derive(Clone, Debug, Default)]
pub struct PersonalHistory {
    undo: VecDeque<UndoAction>,
    redo: Vec<UndoAction>,
    retained_bytes: usize,
    pending: Option<PendingMove>,
}

impl PersonalHistory {
    pub fn record(&mut self, action: UndoAction) {
        let redo_bytes = self.redo.iter().fold(0usize, |bytes, entry| bytes.saturating_add(entry.retained_bytes));
        self.retained_bytes = self.retained_bytes.saturating_sub(redo_bytes);
        self.redo.clear();
        if action.retained_bytes > PERSONAL_HISTORY_BYTES {
            return;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(action.retained_bytes);
        self.undo.push_back(action);
        while self.undo.len() > PERSONAL_HISTORY_ACTIONS || self.retained_bytes > PERSONAL_HISTORY_BYTES {
            if let Some(evicted) = self.undo.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes);
            }
        }
    }

    pub fn prepare_undo(&mut self) -> Result<GuardedPatch, HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Pending);
        }
        let action = self.undo.back().ok_or(HistoryError::NothingToUndo)?;
        self.pending = Some(PendingMove { direction: HistoryDirection::Undo, source_action: action.action });
        Ok(action.undo.clone())
    }

    pub fn prepare_redo(&mut self) -> Result<GuardedPatch, HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Pending);
        }
        let action = self.redo.last().ok_or(HistoryError::NothingToRedo)?;
        self.pending = Some(PendingMove { direction: HistoryDirection::Redo, source_action: action.action });
        Ok(action.redo.clone())
    }

    pub fn accepted(&mut self, _action: u64) {
        let Some(pending) = self.pending.take() else { return };
        match pending.direction {
            HistoryDirection::Undo => {
                if self.undo.back().is_some_and(|action| action.action == pending.source_action)
                    && let Some(action) = self.undo.pop_back()
                {
                    self.redo.push(action);
                }
            }
            HistoryDirection::Redo => {
                if self.redo.last().is_some_and(|action| action.action == pending.source_action)
                    && let Some(action) = self.redo.pop()
                {
                    self.undo.push_back(action);
                }
            }
        }
    }

    pub fn refused(&mut self, _action: u64) {
        self.pending = None;
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    pub fn has_undo_action(&self, action: u64) -> bool {
        self.undo.iter().any(|candidate| candidate.action == action)
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.retained_bytes = 0;
        self.pending = None;
    }
}

fn content_input(ids: &WorkbookIds, wb: &Workbook, sheet: usize, at: CellRef) -> Result<GuardedCellInput, CoauthorError> {
    let Some(target_sheet) = wb.sheet(sheet) else { return Err(CoauthorError::UnknownIdentity(format!("sheet position {sheet}"))) };
    let Some(cell) = target_sheet.cell(at) else {
        return Ok(GuardedCellInput { input: String::new(), formula: false, references: Vec::new() });
    };
    if cell.py.is_some() {
        return Err(CoauthorError::MalformedPayload("Python cell state cannot enter a guarded patch".into()));
    }
    let input = stable_content_input(cell);
    let (input, formula, references) = encode_formula_in_workbook(ids, wb, sheet, at, &input, cell.formula.is_some())?;
    Ok(GuardedCellInput { input, formula, references })
}

fn stable_content_input(cell: &Cell) -> String {
    if cell.formula.is_none()
        && let Value::Text(text) = &cell.value
    {
        return format!("'{text}");
    }
    if cell.formula.is_none()
        && let Value::Number(number) = &cell.value
    {
        return number.to_string();
    }
    cell.input_text()
}

fn is_content(command: &SharedCommand) -> bool {
    matches!(command, SharedCommand::CellSet { .. } | SharedCommand::ClearContents { .. })
}

fn is_format(command: &SharedCommand) -> bool {
    matches!(
        command,
        SharedCommand::NumberFormat { .. }
            | SharedCommand::FontName { .. }
            | SharedCommand::FontSize { .. }
            | SharedCommand::FontColor { .. }
            | SharedCommand::FillColor { .. }
            | SharedCommand::Borders { .. }
            | SharedCommand::AlignTop { .. }
            | SharedCommand::AlignMiddle { .. }
            | SharedCommand::AlignBottom { .. }
            | SharedCommand::Bold { .. }
            | SharedCommand::Italic { .. }
            | SharedCommand::WrapText { .. }
            | SharedCommand::AlignLeft { .. }
            | SharedCommand::AlignCenter { .. }
            | SharedCommand::AlignRight { .. }
            | SharedCommand::Underline { .. }
            | SharedCommand::DoubleUnderline { .. }
            | SharedCommand::IncreaseFontSize { .. }
            | SharedCommand::DecreaseFontSize { .. }
            | SharedCommand::FormatCells { .. }
    )
}

fn command_sheet(command: &SharedCommand) -> Option<LiveId> {
    match command {
        SharedCommand::CellSet { sheet, .. }
        | SharedCommand::ClearContents { sheet, .. }
        | SharedCommand::NumberFormat { sheet, .. }
        | SharedCommand::FontName { sheet, .. }
        | SharedCommand::FontSize { sheet, .. }
        | SharedCommand::FontColor { sheet, .. }
        | SharedCommand::FillColor { sheet, .. }
        | SharedCommand::Borders { sheet, .. }
        | SharedCommand::AlignTop { sheet, .. }
        | SharedCommand::AlignMiddle { sheet, .. }
        | SharedCommand::AlignBottom { sheet, .. }
        | SharedCommand::Bold { sheet, .. }
        | SharedCommand::Italic { sheet, .. }
        | SharedCommand::WrapText { sheet, .. }
        | SharedCommand::AlignLeft { sheet, .. }
        | SharedCommand::AlignCenter { sheet, .. }
        | SharedCommand::AlignRight { sheet, .. }
        | SharedCommand::Underline { sheet, .. }
        | SharedCommand::DoubleUnderline { sheet, .. }
        | SharedCommand::IncreaseFontSize { sheet, .. }
        | SharedCommand::DecreaseFontSize { sheet, .. }
        | SharedCommand::FormatCells { sheet, .. } => Some(*sheet),
        _ => None,
    }
}

pub(crate) fn build_undo_action(
    action: u64,
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    command: &SharedCommand,
) -> Result<Option<UndoAction>, CoauthorError> {
    if let Some(structural) = build_structural_undo(before_ids, after_ids, before, after, command)? {
        let patch = GuardedPatch::Structural(Box::new(structural));
        return finish_action(action, command_label(command), patch).map(Some);
    }
    let Some(sheet_id) = command_sheet(command) else { return Ok(None) };
    let sheet = after_ids.resolve_sheet(sheet_id).map_err(target_error)?;
    let before_sheet = before.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let after_sheet = after.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let patch = if is_content(command) {
        let mut values = Vec::new();
        for cell in before_sheet.cells.diff(&after_sheet.cells) {
            let expected = content_input(after_ids, after, sheet, cell)?;
            let restore = content_input(before_ids, before, sheet, cell)?;
            if expected != restore {
                values.push(GuardedValue { target: after_ids.cell_id(sheet, cell)?, expected, restore });
            }
        }
        GuardedPatch::Cells(values)
    } else if is_format(command) {
        GuardedPatch::Styles(style_changes(after_ids, before, after, sheet, sheet_id)?)
    } else {
        return Ok(None);
    };
    if patch.field_count() == 0 {
        return Ok(None);
    }
    finish_action(action, command_label(command), patch).map(Some)
}

fn finish_action(action: u64, label: &str, patch: GuardedPatch) -> Result<UndoAction, CoauthorError> {
    let redo = patch.reversed();
    let retained_bytes = serde_json::to_vec(&patch)
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?
        .len()
        .saturating_add(serde_json::to_vec(&redo).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?.len());
    Ok(UndoAction { action, label: label.to_string(), undo: patch, redo, retained_bytes })
}

fn command_label(command: &SharedCommand) -> &'static str {
    match command {
        SharedCommand::InsertRows { .. } => "Insert Rows",
        SharedCommand::DeleteRows { .. } => "Delete Rows",
        SharedCommand::InsertColumns { .. } => "Insert Columns",
        SharedCommand::DeleteColumns { .. } => "Delete Columns",
        SharedCommand::InsertSheet { .. } => "Add Sheet",
        SharedCommand::RenameSheet { .. } => "Rename Sheet",
        SharedCommand::DeleteSheet { .. } => "Delete Sheet",
        _ if is_content(command) => "Cell content",
        _ => "Cell format",
    }
}

fn build_structural_undo(
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    command: &SharedCommand,
) -> Result<Option<GuardedStructural>, CoauthorError> {
    let structural = match command {
        SharedCommand::InsertRows { sheet, rows } => {
            let affected = consecutive_ids(rows.first, rows.count)?;
            let axes = before_ids.by_id.get(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let fillers = tail_ids(&axes.rows, rows.count)?;
            GuardedStructural::InsertRows {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, rows.before, affected, fillers, true, true)?,
            }
        }
        SharedCommand::DeleteRows { sheet, rows } => {
            let fillers = consecutive_ids(
                rows.first,
                u32::try_from(rows.ids.len()).map_err(|_| CoauthorError::MalformedPayload("too many deleted row identities".into()))?,
            )?;
            let before_position = position_after_block(before_ids, *sheet, &rows.ids, true)?;
            GuardedStructural::DeleteRows {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, before_position, rows.ids.clone(), fillers, false, true)?,
            }
        }
        SharedCommand::InsertColumns { sheet, cols } => {
            let affected = consecutive_ids(cols.first, cols.count)?;
            let axes = before_ids.by_id.get(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let fillers = tail_ids(&axes.cols, cols.count)?;
            GuardedStructural::InsertColumns {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, cols.before, affected, fillers, true, false)?,
            }
        }
        SharedCommand::DeleteColumns { sheet, cols } => {
            let fillers = consecutive_ids(
                cols.first,
                u32::try_from(cols.ids.len()).map_err(|_| CoauthorError::MalformedPayload("too many deleted column identities".into()))?,
            )?;
            let before_position = position_after_block(before_ids, *sheet, &cols.ids, false)?;
            GuardedStructural::DeleteColumns {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, before_position, cols.ids.clone(), fillers, false, false)?,
            }
        }
        SharedCommand::InsertSheet { before: insert, .. } => {
            let sheet = after_ids.resolve_sheet(insert.id).map_err(target_error)?;
            GuardedStructural::AddSheet {
                sheet: insert.id,
                before: insert.before,
                expected_present: true,
                entered: sheet_entered(after, sheet)?,
                snapshot: Box::new(guarded_sheet_snapshot(after_ids, after, insert.id)?),
            }
        }
        SharedCommand::RenameSheet { sheet, .. } => {
            let before_sheet = before_ids.resolve_sheet(*sheet).map_err(target_error)?;
            let after_sheet = after_ids.resolve_sheet(*sheet).map_err(target_error)?;
            let restore = before.sheet(before_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?.name.clone();
            let expected = after.sheet(after_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?.name.clone();
            GuardedStructural::RenameSheet { sheet: *sheet, expected, restore }
        }
        SharedCommand::DeleteSheet { sheet } => build_delete_sheet_state(before_ids, after_ids, before, after, *sheet)?,
        _ => return Ok(None),
    };
    Ok(Some(structural))
}

fn consecutive_ids(first: LiveId, count: u32) -> Result<Vec<LiveId>, CoauthorError> {
    let capacity = usize::try_from(count).map_err(|_| CoauthorError::MalformedPayload("identity count is too large".into()))?;
    let mut ids = Vec::with_capacity(capacity);
    for offset in 0..count {
        let counter = first.1.checked_add(offset).ok_or_else(|| CoauthorError::MalformedPayload("identity counter overflows".into()))?;
        ids.push(LiveId(first.0, counter));
    }
    Ok(ids)
}

fn tail_ids(ids: &collab_live::ids::OrderedIds, count: u32) -> Result<Vec<LiveId>, CoauthorError> {
    let start = ids.len().checked_sub(count).ok_or_else(|| CoauthorError::MalformedPayload("structural identity tail is too short".into()))?;
    (start..ids.len()).map(|at| ids.id_at(at).ok_or_else(|| CoauthorError::UnknownIdentity(format!("axis position {at}")))).collect()
}

fn position_after_block(ids: &WorkbookIds, sheet: LiveId, targets: &[LiveId], rows: bool) -> Result<IdentityPosition, CoauthorError> {
    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let ordered = if rows { &axes.rows } else { &axes.cols };
    let mut positions = targets
        .iter()
        .map(|id| match ordered.find(*id) {
            Lookup::At(at) => Ok(at),
            Lookup::Deleted { .. } => Err(CoauthorError::DeletedTarget(format!("axis identity {id:?}"))),
            Lookup::Unknown => Err(CoauthorError::UnknownIdentity(format!("axis identity {id:?}"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    positions.sort_unstable();
    let first = positions.first().copied().ok_or_else(|| CoauthorError::MalformedPayload("structural deletion has no identities".into()))?;
    for (offset, position) in positions.iter().copied().enumerate() {
        let offset = u32::try_from(offset).map_err(|_| CoauthorError::MalformedPayload("too many structural identities".into()))?;
        if position != first.saturating_add(offset) {
            return Err(CoauthorError::MalformedPayload("structural deletion identities are not consecutive".into()));
        }
    }
    let after = first
        .checked_add(u32::try_from(positions.len()).map_err(|_| CoauthorError::MalformedPayload("too many structural identities".into()))?)
        .ok_or_else(|| CoauthorError::MalformedPayload("structural deletion position overflows".into()))?;
    Ok(ordered.id_at(after).map_or(IdentityPosition::End, IdentityPosition::Before))
}

#[allow(clippy::too_many_arguments)]
fn build_axis_state(
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    sheet: LiveId,
    position: IdentityPosition,
    affected: Vec<LiveId>,
    fillers: Vec<LiveId>,
    expected_present: bool,
    rows: bool,
) -> Result<GuardedAxisState, CoauthorError> {
    let (cells, styles, lines) = structural_changes(before_ids, after_ids, before, after)?;
    let (present_ids, present_wb) = if expected_present { (after_ids, after) } else { (before_ids, before) };
    let entered = axis_entered(present_ids, present_wb, sheet, &affected, rows)?;
    let (filler_ids, filler_wb) = if expected_present { (before_ids, before) } else { (after_ids, after) };
    let filler_entered = axis_entered(filler_ids, filler_wb, sheet, &fillers, rows)?;
    Ok(GuardedAxisState { sheet, before: position, affected, fillers, expected_present, entered, filler_entered, cells, styles, lines })
}

fn build_delete_sheet_state(
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    sheet: LiveId,
) -> Result<GuardedStructural, CoauthorError> {
    let before_sheet = before_ids.resolve_sheet(sheet).map_err(target_error)?;
    let before_position = before_ids
        .sheets
        .id_at(u32::try_from(before_sheet.saturating_add(1)).map_err(|_| CoauthorError::MalformedPayload("sheet position is too large".into()))?);
    let before_position = before_position.map_or(IdentityPosition::End, IdentityPosition::Before);
    let mut before_states = vec![guarded_sheet_state(before_ids, before, sheet)?];
    let mut after_states = Vec::new();
    for at in 0..after_ids.sheets.len() {
        let id = after_ids.sheets.id_at(at).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet position {at}")))?;
        before_states.push(guarded_sheet_state(before_ids, before, id)?);
        after_states.push(guarded_sheet_state(after_ids, after, id)?);
    }
    Ok(GuardedStructural::WholeAction {
        label: "Delete Sheet".into(),
        sheet,
        before: before_position,
        expected_after: true,
        before_state: Box::new(GuardedWholeState { sheets: before_states, names: before.names.clone() }),
        after_state: Box::new(GuardedWholeState { sheets: after_states, names: after.names.clone() }),
    })
}

fn guarded_sheet_state(ids: &WorkbookIds, wb: &Workbook, sheet: LiveId) -> Result<GuardedSheetState, CoauthorError> {
    let at = ids.resolve_sheet(sheet).map_err(target_error)?;
    let at_u32 = u32::try_from(at).map_err(|_| CoauthorError::MalformedPayload("sheet position is too large".into()))?;
    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    Ok(GuardedSheetState {
        sheet,
        at: at_u32,
        entered: sheet_entered(wb, at)?,
        rows: axes.rows.clone(),
        cols: axes.cols.clone(),
        snapshot: guarded_sheet_snapshot(ids, wb, sheet)?,
    })
}

fn guarded_sheet_snapshot(ids: &WorkbookIds, wb: &Workbook, sheet: LiveId) -> Result<GuardedSheetSnapshot, CoauthorError> {
    let at = ids.resolve_sheet(sheet).map_err(target_error)?;
    let target = wb.sheet(at).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let mut formulas = Vec::new();
    for (cell, value) in target.cells.iter() {
        if value.formula.is_some() {
            formulas.push(GuardedSheetFormula { target: ids.cell_id(at, cell)?, input: content_input(ids, wb, at, cell)? });
        }
    }
    Ok(GuardedSheetSnapshot { sheet: target.clone(), file_id: target.file_id, formulas })
}

fn sheet_entered(wb: &Workbook, sheet: usize) -> Result<[u8; 32], CoauthorError> {
    let target = wb.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet position {sheet}")))?;
    let mut isolated = wb.clone();
    isolated.sheets = vec![Arc::new(target.clone())];
    isolated.names.clear();
    Ok(crate::fingerprint::workbook_fingerprint(&isolated).entered)
}

type StructuralChanges = (Vec<GuardedCellContent>, Vec<GuardedStyleLeaf>, Vec<GuardedLineState>);

fn structural_changes(
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
) -> Result<StructuralChanges, CoauthorError> {
    let before_cells = cell_inputs(before_ids, before)?;
    let after_cells = cell_inputs(after_ids, after)?;
    let empty = GuardedCellInput { input: String::new(), formula: false, references: Vec::new() };
    let targets: BTreeSet<CellId> = before_cells.keys().chain(after_cells.keys()).copied().collect();
    let mut cells = Vec::new();
    for target in &targets {
        let restore = before_cells.get(target).unwrap_or(&empty);
        let expected = after_cells.get(target).unwrap_or(&empty);
        if restore != expected {
            cells.push(GuardedValue { target: *target, expected: expected.clone(), restore: restore.clone() });
        }
    }

    let before_styles = cell_styles(before_ids, before)?;
    let after_styles = cell_styles(after_ids, after)?;
    let default_style = style_json(&Style::default())?;
    let mut styles = Vec::new();
    for target in targets {
        append_style_leaves(
            StyleTarget::Cell { cell: target },
            before_styles.get(&target).cloned().unwrap_or_else(|| default_style.clone()),
            after_styles.get(&target).cloned().unwrap_or_else(|| default_style.clone()),
            &mut styles,
        )?;
    }

    let before_lines = line_states(before_ids, before)?;
    let after_lines = line_states(after_ids, after)?;
    let line_targets: BTreeSet<StyleTarget> = before_lines.keys().chain(after_lines.keys()).copied().collect();
    let mut lines = Vec::new();
    for target in line_targets {
        let restore = before_lines.get(&target).copied();
        let expected = after_lines.get(&target).copied();
        if restore != expected {
            lines.push(GuardedLineState { target, expected, restore });
        }
    }
    Ok((cells, styles, lines))
}

fn cell_inputs(ids: &WorkbookIds, wb: &Workbook) -> Result<BTreeMap<CellId, GuardedCellInput>, CoauthorError> {
    let mut values = BTreeMap::new();
    for (sheet, target_sheet) in wb.sheets.iter().enumerate() {
        for (cell, _) in target_sheet.cells.iter() {
            values.insert(ids.cell_id(sheet, cell)?, content_input(ids, wb, sheet, cell)?);
        }
    }
    Ok(values)
}

fn cell_styles(ids: &WorkbookIds, wb: &Workbook) -> Result<BTreeMap<CellId, Json>, CoauthorError> {
    let mut values = BTreeMap::new();
    for (sheet, target_sheet) in wb.sheets.iter().enumerate() {
        for (cell, _) in target_sheet.cells.iter() {
            values.insert(ids.cell_id(sheet, cell)?, style_json(wb.styles.get(target_sheet.style_id(cell)))?);
        }
    }
    Ok(values)
}

fn line_states(ids: &WorkbookIds, wb: &Workbook) -> Result<BTreeMap<StyleTarget, LineInfo>, CoauthorError> {
    let mut values = BTreeMap::new();
    for (sheet, target_sheet) in wb.sheets.iter().enumerate() {
        let sheet_id = ids.sheet_id(sheet)?;
        let axes = ids.by_id.get(&sheet_id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
        for (row, info) in &target_sheet.rows {
            let row = axes.rows.id_at(*row).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row position {row}")))?;
            values.insert(StyleTarget::Row { sheet: sheet_id, row }, *info);
        }
        for (col, info) in &target_sheet.cols {
            let col = axes.cols.id_at(*col).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column position {col}")))?;
            values.insert(StyleTarget::Column { sheet: sheet_id, col }, *info);
        }
    }
    Ok(values)
}

fn axis_entered(ids: &WorkbookIds, wb: &Workbook, sheet: LiveId, affected: &[LiveId], rows: bool) -> Result<[u8; 32], CoauthorError> {
    let sheet_index = ids.resolve_sheet(sheet).map_err(target_error)?;
    let target_sheet = wb.sheet(sheet_index).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let wanted: BTreeSet<LiveId> = affected.iter().copied().collect();
    if wanted.len() != affected.len() {
        return Err(CoauthorError::MalformedPayload("structural guard repeats an identity".into()));
    }
    let mut cells = Vec::new();
    for (cell, _) in target_sheet.cells.iter() {
        let target = ids.cell_id(sheet_index, cell)?;
        let axis_id = if rows { target.row } else { target.col };
        if wanted.contains(&axis_id) {
            cells.push((target, content_input(ids, wb, sheet_index, cell)?, style_json(wb.styles.get(target_sheet.style_id(cell)))?));
        }
    }
    let mut lines = Vec::new();
    for id in affected {
        let at = match if rows { axes.rows.find(*id) } else { axes.cols.find(*id) } {
            Lookup::At(at) => at,
            Lookup::Deleted { .. } => return Err(CoauthorError::DeletedTarget(format!("axis identity {id:?}"))),
            Lookup::Unknown => return Err(CoauthorError::UnknownIdentity(format!("axis identity {id:?}"))),
        };
        let info = if rows { target_sheet.rows.get(&at) } else { target_sheet.cols.get(&at) }.copied();
        let style = info.and_then(|line| line.style).map(|style| style_json(wb.styles.get(style))).transpose()?;
        lines.push((*id, info, style));
    }
    let bytes = serde_json::to_vec(&(cells, lines)).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    let mut entered = [0u8; 32];
    entered.copy_from_slice(&digest);
    Ok(entered)
}

fn target_error(error: TargetGone) -> CoauthorError {
    match error {
        TargetGone::DeletedSheet(id) => CoauthorError::DeletedTarget(format!("sheet {id:?}")),
        TargetGone::DeletedRow(id) => CoauthorError::DeletedTarget(format!("row {id:?}")),
        TargetGone::DeletedColumn(id) => CoauthorError::DeletedTarget(format!("column {id:?}")),
        TargetGone::UnknownSheet(id) => CoauthorError::UnknownIdentity(format!("sheet {id:?}")),
        TargetGone::UnknownRow(id) => CoauthorError::UnknownIdentity(format!("row {id:?}")),
        TargetGone::UnknownColumn(id) => CoauthorError::UnknownIdentity(format!("column {id:?}")),
        TargetGone::Malformed(message) => CoauthorError::MalformedPayload(message),
        TargetGone::UnrepresentableReference => CoauthorError::MalformedPayload("unrepresentable identity reference".into()),
    }
}

fn style_changes(
    ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    sheet: usize,
    sheet_id: LiveId,
) -> Result<Vec<GuardedStyleLeaf>, CoauthorError> {
    let before_sheet = before.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let after_sheet = after.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let mut changes = Vec::new();
    for cell in before_sheet.cells.diff(&after_sheet.cells) {
        append_style_leaves(
            StyleTarget::Cell { cell: ids.cell_id(sheet, cell)? },
            style_json(before.styles.get(before_sheet.style_id(cell)))?,
            style_json(after.styles.get(after_sheet.style_id(cell)))?,
            &mut changes,
        )?;
    }
    let axes = ids.by_id.get(&sheet_id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let row_keys: BTreeSet<u32> = before_sheet.rows.keys().chain(after_sheet.rows.keys()).copied().collect();
    for row in row_keys {
        let before_id = before_sheet.rows.get(&row).and_then(|line| line.style).unwrap_or_default();
        let after_id = after_sheet.rows.get(&row).and_then(|line| line.style).unwrap_or_default();
        if before_id == after_id {
            continue;
        }
        let row_id = axes.rows.id_at(row).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row position {row}")))?;
        append_style_leaves(
            StyleTarget::Row { sheet: sheet_id, row: row_id },
            style_json(before.styles.get(before_id))?,
            style_json(after.styles.get(after_id))?,
            &mut changes,
        )?;
    }
    let col_keys: BTreeSet<u32> = before_sheet.cols.keys().chain(after_sheet.cols.keys()).copied().collect();
    for col in col_keys {
        let before_id = before_sheet.cols.get(&col).and_then(|line| line.style).unwrap_or_default();
        let after_id = after_sheet.cols.get(&col).and_then(|line| line.style).unwrap_or_default();
        if before_id == after_id {
            continue;
        }
        let col_id = axes.cols.id_at(col).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column position {col}")))?;
        append_style_leaves(
            StyleTarget::Column { sheet: sheet_id, col: col_id },
            style_json(before.styles.get(before_id))?,
            style_json(after.styles.get(after_id))?,
            &mut changes,
        )?;
    }
    Ok(changes)
}

fn style_json(style: &Style) -> Result<Json, CoauthorError> {
    serde_json::to_value(style).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

fn append_style_leaves(target: StyleTarget, restore: Json, expected: Json, out: &mut Vec<GuardedStyleLeaf>) -> Result<(), CoauthorError> {
    let mut leaves = Vec::new();
    changed_leaves(&restore, &expected, &mut Vec::new(), &mut leaves);
    for (path, restore, expected) in leaves {
        let expected = style_leaf_patch(&path, expected)?;
        let restore = style_leaf_patch(&path, restore)?;
        let (expected_path, _) = style_patch_leaf(&expected).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        let (restore_path, _) = style_patch_leaf(&restore).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        if style_model_path(&expected_path) != path || style_model_path(&restore_path) != path {
            return Err(CoauthorError::MalformedPayload("guarded style leaves do not name the same field".into()));
        }
        out.push(GuardedStyleLeaf { target, expected, restore, path });
    }
    Ok(())
}

fn style_leaf_patch(path: &[String], leaf: Json) -> Result<StylePatch, CoauthorError> {
    let Some((first, rest)) = path.split_first() else {
        return Err(CoauthorError::MalformedPayload("guarded style path is empty".into()));
    };
    let mut nested = leaf;
    for part in rest.iter().rev() {
        let mut object = serde_json::Map::new();
        object.insert(part.clone(), nested);
        nested = Json::Object(object);
    }
    let mut root = serde_json::Map::new();
    root.insert(first.clone(), nested);
    serde_json::from_value(Json::Object(root)).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

fn style_patch_leaf(patch: &StylePatch) -> Result<(Vec<String>, Json), EngineError> {
    fn collect(value: &Json, path: &mut Vec<String>, leaves: &mut Vec<(Vec<String>, Json)>) {
        if atomic_style_path(path) {
            leaves.push((path.clone(), value.clone()));
            return;
        }
        match value {
            Json::Object(object) if !object.is_empty() => {
                for (key, value) in object {
                    path.push(key.clone());
                    collect(value, path, leaves);
                    path.pop();
                }
            }
            _ => leaves.push((path.clone(), value.clone())),
        }
    }

    let value = serde_json::to_value(patch).map_err(|error| EngineError::Other(error.to_string()))?;
    let mut leaves = Vec::new();
    collect(&value, &mut Vec::new(), &mut leaves);
    if leaves.len() != 1 {
        return Err(EngineError::Other("a guarded style value must contain exactly one typed style leaf".into()));
    }
    leaves.pop().ok_or_else(|| EngineError::Other("guarded style value is empty".into()))
}

fn atomic_style_path(path: &[String]) -> bool {
    path.last().is_some_and(|part| matches!(part.as_str(), "color" | "fg" | "bg"))
}

fn style_model_path(path: &[String]) -> Vec<String> {
    path.iter()
        .map(|part| match part.as_str() {
            "numFmt" => "num_fmt".to_string(),
            "diagDown" => "diag_down".to_string(),
            "diagUp" => "diag_up".to_string(),
            "readingOrder" => "reading_order".to_string(),
            _ => part.clone(),
        })
        .collect()
}

fn changed_leaves(before: &Json, after: &Json, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, Json, Json)>) {
    if before == after {
        return;
    }
    match (before, after) {
        _ if atomic_style_path(path) => out.push((path.clone(), before.clone(), after.clone())),
        (Json::Object(before), Json::Object(after)) => {
            let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            for key in keys {
                path.push(key.clone());
                changed_leaves(before.get(key).unwrap_or(&Json::Null), after.get(key).unwrap_or(&Json::Null), path, out);
                path.pop();
            }
        }
        _ => out.push((path.clone(), before.clone(), after.clone())),
    }
}

#[derive(Clone, Debug)]
enum ResolvedGuard {
    CellContent { sheet: usize, cell: CellRef, expected: String, expected_formula: bool, restore: String, restore_formula: bool },
    Style { target: ResolvedStyleTarget, path: Vec<String>, expected: Json, restore: Json },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ResolvedStyleTarget {
    Cell { sheet: usize, cell: CellRef },
    Row { sheet: usize, row: u32 },
    Column { sheet: usize, col: u32 },
}

pub(crate) fn apply_guarded_patch(session: &mut Session, ids: &mut WorkbookIds, patch: &GuardedPatch) -> Result<UndoReport, EngineError> {
    validate_guarded_patch(patch)?;
    if let GuardedPatch::Structural(structural) = patch {
        return apply_structural_patch(session, ids, structural);
    }
    let mut guards = Vec::new();
    let mut left = 0usize;
    resolve_guards(ids, session, patch, &mut guards, &mut left)?;
    check_guard_protection(session, &guards)?;
    let line_cells = index_guarded_line_cells(&session.doc()?.wb, &guards);
    let restored = crate::cmd::edit(session, |cx| {
        let mut restored = 0usize;
        let mut changed = BTreeSet::new();
        let mut fit_rows = BTreeSet::new();
        for guard in &guards {
            match guard {
                ResolvedGuard::CellContent { sheet, cell, expected, expected_formula, restore, restore_formula } => {
                    let current = cx.wb.sheet(*sheet).and_then(|sheet| sheet.cell(*cell));
                    let current_is_python = current.is_some_and(|cell| cell.py.is_some());
                    let current_formula = current.is_some_and(|cell| cell.formula.is_some());
                    let current_input = current.filter(|cell| cell.py.is_none()).map(stable_content_input).unwrap_or_default();
                    if current_is_python || current_formula != *expected_formula || &current_input != expected {
                        left = left.saturating_add(1);
                        continue;
                    }
                    let style = cx.wb.sheet(*sheet).map(|sheet| sheet.style_id(*cell)).unwrap_or_default();
                    let replacement = guarded_input_to_cell(restore, *restore_formula, style, &mut cx.wb)?;
                    match replacement {
                        Some(mut replacement) => {
                            replacement.style = style;
                            cx.sheet_mut(*sheet)?.set_cell(*cell, replacement);
                        }
                        None => {
                            cx.sheet_mut(*sheet)?.remove_cell(*cell);
                        }
                    }
                    changed.insert((*sheet, *cell));
                    restored = restored.saturating_add(1);
                }
                ResolvedGuard::Style { target, path, expected, restore } => {
                    let current_style = resolved_style(&cx.wb, *target)?;
                    let mut current = serde_json::to_value(current_style).map_err(|error| EngineError::Other(error.to_string()))?;
                    if json_leaf(&current, path) != Some(expected) {
                        left = left.saturating_add(1);
                        continue;
                    }
                    set_json_leaf(&mut current, path, restore.clone())?;
                    let style: Style = serde_json::from_value(current).map_err(|error| EngineError::Other(error.to_string()))?;
                    let style_id = cx.wb.styles.intern(style);
                    set_resolved_style(cx, *target, style_id)?;
                    match target {
                        ResolvedStyleTarget::Cell { sheet, cell } => {
                            changed.insert((*sheet, *cell));
                            fit_rows.insert((*sheet, cell.row));
                        }
                        ResolvedStyleTarget::Row { sheet, row } => {
                            for cell in line_cells.get(target).into_iter().flatten() {
                                changed.insert((*sheet, *cell));
                            }
                            fit_rows.insert((*sheet, *row));
                        }
                        ResolvedStyleTarget::Column { sheet, .. } => {
                            for cell in line_cells.get(target).into_iter().flatten() {
                                changed.insert((*sheet, *cell));
                                fit_rows.insert((*sheet, cell.row));
                            }
                        }
                    }
                    restored = restored.saturating_add(1);
                }
            }
        }
        cx.changed.extend(changed);
        cx.fit_rows.extend(fit_rows);
        Ok(restored)
    })?;
    let reason = (left > 0).then(|| "Some fields were left because they changed after this action.".to_string());
    Ok(UndoReport { restored, left, reason })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GuardKey {
    Content(CellId),
    Style(StyleTarget, Vec<String>),
    Line(StyleTarget),
}

#[derive(Default)]
struct GuardValidator {
    count: usize,
    keys: BTreeSet<GuardKey>,
}

impl GuardValidator {
    fn add(&mut self, key: GuardKey) -> Result<(), EngineError> {
        self.count_items(1)?;
        if !self.keys.insert(key) {
            return Err(EngineError::Other("a guarded patch repeats a target and path".into()));
        }
        Ok(())
    }

    fn count_items(&mut self, count: usize) -> Result<(), EngineError> {
        self.count = self.count.saturating_add(count);
        if self.count > MAX_GUARDS_PER_ENTRY {
            return Err(EngineError::Other(format!("a guarded patch may contain at most {MAX_GUARDS_PER_ENTRY} guards")));
        }
        Ok(())
    }
}

fn validate_guarded_patch(patch: &GuardedPatch) -> Result<(), EngineError> {
    let mut validator = GuardValidator::default();
    validate_patch_fields(patch, &mut validator)
}

fn validate_patch_fields(patch: &GuardedPatch, validator: &mut GuardValidator) -> Result<(), EngineError> {
    match patch {
        GuardedPatch::Cells(values) => validate_cell_guards(values, validator)?,
        GuardedPatch::Styles(values) => validate_style_guards(values, validator)?,
        GuardedPatch::Composite(values) => {
            for value in values {
                validate_patch_fields(value, validator)?;
            }
        }
        GuardedPatch::Structural(structural) => validate_structural_guards(structural, validator)?,
    }
    Ok(())
}

fn validate_structural_guards(structural: &GuardedStructural, validator: &mut GuardValidator) -> Result<(), EngineError> {
    match structural {
        GuardedStructural::InsertRows { state }
        | GuardedStructural::DeleteRows { state }
        | GuardedStructural::InsertColumns { state }
        | GuardedStructural::DeleteColumns { state } => {
            validator.count_items(state.affected.len().saturating_add(state.fillers.len()))?;
            let affected: BTreeSet<LiveId> = state.affected.iter().copied().collect();
            let fillers: BTreeSet<LiveId> = state.fillers.iter().copied().collect();
            if affected.len() != state.affected.len() || fillers.len() != state.fillers.len() {
                return Err(EngineError::Other("a structural guarded patch repeats an identity".into()));
            }
            validate_cell_guards(&state.cells, validator)?;
            validate_style_guards(&state.styles, validator)?;
            for line in &state.lines {
                validator.add(GuardKey::Line(line.target))?;
            }
        }
        GuardedStructural::AddSheet { snapshot, .. } => {
            validator.count_items(1)?;
            validate_snapshot_formula_guards(snapshot, validator)?;
        }
        GuardedStructural::RenameSheet { .. } => {
            validator.count_items(1)?;
        }
        GuardedStructural::WholeAction { before_state, after_state, .. } => {
            validate_whole_state_guards(before_state, validator)?;
            validate_whole_state_guards(after_state, validator)?;
        }
    }
    Ok(())
}

fn validate_cell_guards(values: &[GuardedCellContent], validator: &mut GuardValidator) -> Result<(), EngineError> {
    for value in values {
        validator.add(GuardKey::Content(value.target))?;
    }
    Ok(())
}

fn validate_style_guards(values: &[GuardedStyleLeaf], validator: &mut GuardValidator) -> Result<(), EngineError> {
    for value in values {
        validator.add(GuardKey::Style(value.target, value.path.clone()))?;
    }
    Ok(())
}

fn validate_whole_state_guards(state: &GuardedWholeState, validator: &mut GuardValidator) -> Result<(), EngineError> {
    let mut sheets = BTreeSet::new();
    for guarded in &state.sheets {
        if !sheets.insert(guarded.sheet) {
            return Err(EngineError::Other("a guarded whole-workbook state repeats a sheet".into()));
        }
        validator.count_items(1)?;
        validate_snapshot_formula_guards(&guarded.snapshot, validator)?;
    }
    Ok(())
}

fn validate_snapshot_formula_guards(snapshot: &GuardedSheetSnapshot, validator: &mut GuardValidator) -> Result<(), EngineError> {
    let mut targets = BTreeSet::new();
    for formula in &snapshot.formulas {
        if !targets.insert(formula.target) {
            return Err(EngineError::Other("a guarded sheet snapshot repeats a formula target".into()));
        }
        validator.count_items(1)?;
    }
    Ok(())
}

fn check_guard_protection(session: &Session, guards: &[ResolvedGuard]) -> Result<(), EngineError> {
    let wb = &session.doc()?.wb;
    for guard in guards {
        let ResolvedGuard::Style { target, path, .. } = guard else { continue };
        let sheet = match target {
            ResolvedStyleTarget::Cell { sheet, .. } | ResolvedStyleTarget::Row { sheet, .. } | ResolvedStyleTarget::Column { sheet, .. } => *sheet,
        };
        let Some(protection) = wb.sheet(sheet).and_then(|sheet| sheet.protection.as_ref()) else { continue };
        if !protection.format_cells {
            return Err(EngineError::Other("The cell or chart you're trying to change is on a protected sheet.".into()));
        }
        if path.first().is_some_and(|part| part == "protection") {
            return Err(EngineError::Other("You can't change cell protection on a protected sheet. Unprotect it first.".into()));
        }
    }
    Ok(())
}

fn index_guarded_line_cells(wb: &Workbook, guards: &[ResolvedGuard]) -> BTreeMap<ResolvedStyleTarget, Vec<CellRef>> {
    let mut wanted: BTreeMap<usize, (BTreeSet<u32>, BTreeSet<u32>)> = BTreeMap::new();
    for guard in guards {
        let ResolvedGuard::Style { target, .. } = guard else { continue };
        match target {
            ResolvedStyleTarget::Row { sheet, row } => {
                wanted.entry(*sheet).or_default().0.insert(*row);
            }
            ResolvedStyleTarget::Column { sheet, col } => {
                wanted.entry(*sheet).or_default().1.insert(*col);
            }
            ResolvedStyleTarget::Cell { .. } => {}
        }
    }
    let mut indexed = BTreeMap::new();
    for (sheet_index, (rows, cols)) in wanted {
        let Some(sheet) = wb.sheet(sheet_index) else { continue };
        for (cell, _) in sheet.cells.iter() {
            if rows.contains(&cell.row) {
                indexed.entry(ResolvedStyleTarget::Row { sheet: sheet_index, row: cell.row }).or_insert_with(Vec::new).push(cell);
            }
            if cols.contains(&cell.col) {
                indexed.entry(ResolvedStyleTarget::Column { sheet: sheet_index, col: cell.col }).or_insert_with(Vec::new).push(cell);
            }
        }
    }
    indexed
}

fn guarded_input_to_cell(input: &str, formula: bool, style: StyleId, wb: &mut Workbook) -> Result<Option<Cell>, EngineError> {
    let mut cell = if formula {
        if gridcraft_model::python::is_py_formula(input) {
            return Err(EngineError::Other("Python cells cannot enter a guarded patch".into()));
        }
        let body = input.strip_prefix('=').unwrap_or(input);
        let expression = wb.name_call_case(crate::cmd::edit::parse_typed_formula(body)?);
        crate::cmd::edit::check_arg_counts(&expression)?;
        Cell::formula(Formula::from_expr(expression))
    } else if input.is_empty() {
        Cell::value(Value::Empty)
    } else if let Some(text) = input.strip_prefix('\'') {
        Cell::value(Value::text(text))
    } else {
        Cell::value(gridcraft_core::parse::parse_input(input, wb.date_system).value)
    };
    if !formula && stable_content_input(&cell) != input {
        return Err(EngineError::Other("guarded literal input is not in canonical typed form".into()));
    }
    cell.style = style;
    Ok((!cell.is_blank()).then_some(cell))
}

fn resolve_guards(
    ids: &WorkbookIds,
    session: &Session,
    patch: &GuardedPatch,
    out: &mut Vec<ResolvedGuard>,
    left: &mut usize,
) -> Result<(), EngineError> {
    match patch {
        GuardedPatch::Cells(values) => {
            for value in values {
                match resolve_cell_input(ids, session, value.target, &value.expected.input, value.expected.formula, &value.expected.references) {
                    Ok((sheet, cell, expected)) => {
                        let (_, restore_cell, restore) =
                            resolve_cell_input(ids, session, value.target, &value.restore.input, value.restore.formula, &value.restore.references)
                                .map_err(|error| EngineError::Other(target_error(error).to_string()))?;
                        if restore_cell != cell {
                            return Err(EngineError::Other("guarded cell values resolve to different targets".into()));
                        }
                        out.push(ResolvedGuard::CellContent {
                            sheet,
                            cell,
                            expected,
                            expected_formula: value.expected.formula,
                            restore,
                            restore_formula: value.restore.formula,
                        });
                    }
                    Err(TargetGone::DeletedSheet(_) | TargetGone::DeletedRow(_) | TargetGone::DeletedColumn(_)) => {
                        *left = left.saturating_add(1);
                    }
                    Err(error) => return Err(EngineError::Other(target_error(error).to_string())),
                }
            }
        }
        GuardedPatch::Styles(values) => {
            for value in values {
                match resolve_style_target(ids, value.target) {
                    Ok(target) => {
                        let wb = &session.doc()?.wb;
                        let mut style = serde_json::to_value(resolved_style(wb, target)?).map_err(|error| EngineError::Other(error.to_string()))?;
                        let (expected_path, expected) = style_patch_leaf(&value.expected)?;
                        let (restore_path, restore) = style_patch_leaf(&value.restore)?;
                        if style_model_path(&expected_path) != value.path || style_model_path(&restore_path) != value.path {
                            return Err(EngineError::Other("guarded style path does not match its typed style values".into()));
                        }
                        let mut expected_style = style.clone();
                        set_json_leaf(&mut expected_style, &value.path, expected.clone())?;
                        let _: Style = serde_json::from_value(expected_style).map_err(|error| EngineError::Other(error.to_string()))?;
                        set_json_leaf(&mut style, &value.path, restore.clone())?;
                        let _: Style = serde_json::from_value(style).map_err(|error| EngineError::Other(error.to_string()))?;
                        out.push(ResolvedGuard::Style { target, path: value.path.clone(), expected, restore });
                    }
                    Err(TargetGone::DeletedSheet(_) | TargetGone::DeletedRow(_) | TargetGone::DeletedColumn(_)) => {
                        *left = left.saturating_add(1);
                    }
                    Err(error) => return Err(EngineError::Other(target_error(error).to_string())),
                }
            }
        }
        GuardedPatch::Composite(values) => {
            for value in values {
                resolve_guards(ids, session, value, out, left)?;
            }
        }
        GuardedPatch::Structural(_) => return Err(EngineError::Other("a structural guard must be applied as the outer patch".into())),
    }
    Ok(())
}

fn apply_structural_patch(session: &mut Session, ids: &mut WorkbookIds, structural: &GuardedStructural) -> Result<UndoReport, EngineError> {
    match structural {
        GuardedStructural::InsertRows { state } | GuardedStructural::DeleteRows { state } => apply_axis_patch(session, ids, state, true),
        GuardedStructural::InsertColumns { state } | GuardedStructural::DeleteColumns { state } => apply_axis_patch(session, ids, state, false),
        GuardedStructural::AddSheet { sheet, before, expected_present, entered, snapshot } => {
            apply_add_sheet(session, ids, *sheet, *before, *expected_present, *entered, snapshot)
        }
        GuardedStructural::RenameSheet { sheet, expected, restore } => apply_rename_sheet(session, ids, *sheet, expected, restore),
        GuardedStructural::WholeAction { label, sheet, before, expected_after, before_state, after_state } => {
            apply_whole_action(session, ids, label, *sheet, *before, *expected_after, before_state, after_state)
        }
    }
}

fn apply_axis_patch(session: &mut Session, ids: &mut WorkbookIds, state: &GuardedAxisState, rows: bool) -> Result<UndoReport, EngineError> {
    let count = u32::try_from(state.affected.len()).map_err(|_| EngineError::Other("structural identity count is too large".into()))?;
    if count == 0 || state.fillers.len() != state.affected.len() {
        return Err(EngineError::Other("structural identity exchange is not balanced".into()));
    }
    let sheet_index = ids.resolve_sheet(state.sheet).map_err(|error| EngineError::Other(target_error(error).to_string()))?;
    if state.expected_present {
        let positions = live_axis_positions(ids, state.sheet, &state.affected, rows)?;
        let Some(at) = positions.first().copied() else { return Err(EngineError::Other("structural identity block is empty".into())) };
        if axis_entered(ids, &session.doc()?.wb, state.sheet, &state.affected, rows).map_err(|error| EngineError::Other(error.to_string()))?
            != state.entered
        {
            return Ok(untouched_refusal(if rows { "rows" } else { "columns" }));
        }
        let command = if rows { "home.deleteRows" } else { "home.deleteColumns" };
        let range = axis_range(at, count, rows)?;
        let params = if rows { serde_json::json!({"sheet":sheet_index,"rows":range}) } else { serde_json::json!({"sheet":sheet_index,"cols":range}) };
        session.execute(command, params)?;
        let limit = if rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
        let restore_at = limit.checked_sub(count).ok_or_else(|| EngineError::Other("structural identity count exceeds the worksheet".into()))?;
        if rows {
            ids.exchange_rows(state.sheet, &state.affected, restore_at, &state.fillers)
        } else {
            ids.exchange_columns(state.sheet, &state.affected, restore_at, &state.fillers)
        }
        .map_err(|error| EngineError::Other(error.to_string()))?;
        Ok(UndoReport { restored: 1, left: 0, reason: None })
    } else {
        for id in &state.affected {
            let lookup = ids
                .by_id
                .get(&state.sheet)
                .map(|axes| if rows { axes.rows.find(*id) } else { axes.cols.find(*id) })
                .ok_or_else(|| EngineError::Other(format!("unknown sheet identity {:?}", state.sheet)))?;
            if !matches!(lookup, Lookup::Deleted { .. }) {
                return Ok(untouched_refusal(if rows { "rows" } else { "columns" }));
            }
        }
        let filler_positions = match live_axis_positions(ids, state.sheet, &state.fillers, rows) {
            Ok(positions) => positions,
            Err(_) => return Ok(untouched_refusal(if rows { "rows" } else { "columns" })),
        };
        let limit = if rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
        let expected_tail = limit.checked_sub(count).ok_or_else(|| EngineError::Other("structural identity count exceeds the worksheet".into()))?;
        if filler_positions.first().copied() != Some(expected_tail)
            || axis_entered(ids, &session.doc()?.wb, state.sheet, &state.fillers, rows).map_err(|error| EngineError::Other(error.to_string()))?
                != state.filler_entered
        {
            return Ok(untouched_refusal(if rows { "rows" } else { "columns" }));
        }
        let at = resolve_axis_position(ids, state.sheet, state.before, rows)?;
        let command = if rows { "home.insertRows" } else { "home.insertColumns" };
        let range = axis_range(at, count, rows)?;
        let params = if rows { serde_json::json!({"sheet":sheet_index,"rows":range}) } else { serde_json::json!({"sheet":sheet_index,"cols":range}) };
        session.execute(command, params)?;
        if rows {
            ids.exchange_rows(state.sheet, &state.fillers, at, &state.affected)
        } else {
            ids.exchange_columns(state.sheet, &state.fillers, at, &state.affected)
        }
        .map_err(|error| EngineError::Other(error.to_string()))?;
        let changes = GuardedPatch::Composite(vec![GuardedPatch::Cells(state.cells.clone()), GuardedPatch::Styles(state.styles.clone())]);
        let mut report = apply_guarded_patch(session, ids, &changes)?;
        apply_line_states(session, ids, &state.lines)?;
        report.restored = report.restored.saturating_add(1);
        Ok(report)
    }
}

fn untouched_refusal(target: &str) -> UndoReport {
    UndoReport { restored: 0, left: 1, reason: Some(format!("The {target} changed after this action; the structural change was left untouched.")) }
}

fn live_axis_positions(ids: &WorkbookIds, sheet: LiveId, targets: &[LiveId], rows: bool) -> Result<Vec<u32>, EngineError> {
    let axes = ids.by_id.get(&sheet).ok_or_else(|| EngineError::Other(format!("unknown sheet identity {sheet:?}")))?;
    let ordered = if rows { &axes.rows } else { &axes.cols };
    let mut positions = targets
        .iter()
        .map(|id| match ordered.find(*id) {
            Lookup::At(at) => Ok(at),
            Lookup::Deleted { .. } => Err(EngineError::Other(format!("axis identity {id:?} was deleted"))),
            Lookup::Unknown => Err(EngineError::Other(format!("unknown axis identity {id:?}"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    positions.sort_unstable();
    let Some(first) = positions.first().copied() else { return Ok(positions) };
    for (offset, at) in positions.iter().copied().enumerate() {
        let offset = u32::try_from(offset).map_err(|_| EngineError::Other("too many structural identities".into()))?;
        if at != first.saturating_add(offset) {
            return Err(EngineError::Other("structural identities are no longer consecutive".into()));
        }
    }
    Ok(positions)
}

fn resolve_axis_position(ids: &WorkbookIds, sheet: LiveId, position: IdentityPosition, rows: bool) -> Result<u32, EngineError> {
    let axes = ids.by_id.get(&sheet).ok_or_else(|| EngineError::Other(format!("unknown sheet identity {sheet:?}")))?;
    let ordered = if rows { &axes.rows } else { &axes.cols };
    match position {
        IdentityPosition::End => Ok(ordered.len()),
        IdentityPosition::Before(id) => match ordered.find(id) {
            Lookup::At(at) => Ok(at),
            Lookup::Deleted { .. } => Err(EngineError::Other(format!("structural anchor {id:?} was deleted"))),
            Lookup::Unknown => Err(EngineError::Other(format!("unknown structural anchor {id:?}"))),
        },
    }
}

fn axis_range(at: u32, count: u32, rows: bool) -> Result<String, EngineError> {
    let end = at.checked_add(count).and_then(|end| end.checked_sub(1)).ok_or_else(|| EngineError::Other("structural range overflows".into()))?;
    Ok(if rows { RangeRef::rows(at, end) } else { RangeRef::cols(at, end) }.a1())
}

fn apply_line_states(session: &mut Session, ids: &WorkbookIds, lines: &[GuardedLineState]) -> Result<(), EngineError> {
    crate::cmd::edit(session, |cx| {
        for line in lines {
            match resolve_style_target(ids, line.target).map_err(|error| EngineError::Other(target_error(error).to_string()))? {
                ResolvedStyleTarget::Row { sheet, row } => {
                    if let Some(info) = line.restore {
                        cx.sheet_mut(sheet)?.rows.insert(row, info);
                    } else {
                        cx.sheet_mut(sheet)?.rows.remove(&row);
                    }
                    cx.fit_rows.push((sheet, row));
                }
                ResolvedStyleTarget::Column { sheet, col } => {
                    if let Some(info) = line.restore {
                        cx.sheet_mut(sheet)?.cols.insert(col, info);
                    } else {
                        cx.sheet_mut(sheet)?.cols.remove(&col);
                    }
                }
                ResolvedStyleTarget::Cell { .. } => return Err(EngineError::Other("a guarded line target names a cell".into())),
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn apply_add_sheet(
    session: &mut Session,
    ids: &mut WorkbookIds,
    sheet: LiveId,
    before: IdentityPosition,
    expected_present: bool,
    entered: [u8; 32],
    snapshot: &GuardedSheetSnapshot,
) -> Result<UndoReport, EngineError> {
    if expected_present {
        let at = match ids.resolve_sheet(sheet) {
            Ok(at) => at,
            Err(_) => return Ok(untouched_refusal("added sheet")),
        };
        if sheet_entered(&session.doc()?.wb, at).map_err(|error| EngineError::Other(error.to_string()))? != entered {
            return Ok(untouched_refusal("added sheet"));
        }
        session.execute("home.deleteSheet", serde_json::json!({"sheet":at}))?;
        ids.restore_sheet(Some(sheet), 0, None).map_err(|error| EngineError::Other(error.to_string()))?;
    } else {
        if !matches!(ids.sheets.find(sheet), Lookup::Deleted { .. }) {
            return Ok(untouched_refusal("added sheet"));
        }
        let at = resolve_sheet_position(ids, before)?;
        session.execute("home.insertSheet", serde_json::json!({"name":snapshot.sheet.name,"before":at}))?;
        let identity_at = u32::try_from(at).map_err(|_| EngineError::Other("sheet position is too large".into()))?;
        ids.restore_sheet(None, identity_at, Some(sheet)).map_err(|error| EngineError::Other(error.to_string()))?;
        restore_sheet_snapshot(session, ids, at, snapshot)?;
    }
    Ok(UndoReport { restored: 1, left: 0, reason: None })
}

fn apply_rename_sheet(session: &mut Session, ids: &WorkbookIds, sheet: LiveId, expected: &str, restore: &str) -> Result<UndoReport, EngineError> {
    let at = match ids.resolve_sheet(sheet) {
        Ok(at) => at,
        Err(_) => return Ok(untouched_refusal("renamed sheet")),
    };
    let wb = &session.doc()?.wb;
    if wb.sheet(at).is_none_or(|target| target.name != expected) || wb.sheet_index(restore).is_some_and(|other| other != at) {
        return Ok(untouched_refusal("sheet name"));
    }
    session.execute("sheet.rename", serde_json::json!({"sheet":at,"name":restore}))?;
    Ok(UndoReport { restored: 1, left: 0, reason: None })
}

#[allow(clippy::too_many_arguments)]
fn apply_whole_action(
    session: &mut Session,
    ids: &mut WorkbookIds,
    label: &str,
    sheet: LiveId,
    before: IdentityPosition,
    expected_after: bool,
    before_state: &GuardedWholeState,
    after_state: &GuardedWholeState,
) -> Result<UndoReport, EngineError> {
    let (expected, restore) = if expected_after { (after_state, before_state) } else { (before_state, after_state) };
    if !whole_state_matches(session, ids, sheet, expected)? {
        return Ok(UndoReport {
            restored: 0,
            left: 1,
            reason: Some(format!("{label} can be reversed only while everything it changed is untouched.")),
        });
    }
    let expected_has_target = expected.sheets.iter().any(|state| state.sheet == sheet);
    let restore_target = restore.sheets.iter().find(|state| state.sheet == sheet);
    match (expected_has_target, restore_target) {
        (false, Some(target)) => {
            let at = resolve_sheet_position(ids, before)?;
            session.execute("home.insertSheet", serde_json::json!({"name":target.snapshot.sheet.name,"before":at}))?;
            let identity_at = u32::try_from(at).map_err(|_| EngineError::Other("sheet position is too large".into()))?;
            ids.restore_sheet(None, identity_at, Some(sheet)).map_err(|error| EngineError::Other(error.to_string()))?;
        }
        (true, None) => {
            let at = ids.resolve_sheet(sheet).map_err(|error| EngineError::Other(target_error(error).to_string()))?;
            session.execute("home.deleteSheet", serde_json::json!({"sheet":at}))?;
            ids.restore_sheet(Some(sheet), 0, None).map_err(|error| EngineError::Other(error.to_string()))?;
        }
        (true, Some(_)) | (false, None) => {}
    }
    restore_whole_state(session, ids, restore)?;
    Ok(UndoReport { restored: 1, left: 0, reason: None })
}

fn whole_state_matches(session: &Session, ids: &WorkbookIds, target: LiveId, state: &GuardedWholeState) -> Result<bool, EngineError> {
    if session.doc()?.wb.names != state.names {
        return Ok(false);
    }
    let target_expected = state.sheets.iter().any(|sheet| sheet.sheet == target);
    if target_expected != matches!(ids.sheets.find(target), Lookup::At(_)) {
        return Ok(false);
    }
    for guarded in &state.sheets {
        let at = match ids.resolve_sheet(guarded.sheet) {
            Ok(at) => at,
            Err(_) => return Ok(false),
        };
        if sheet_entered(&session.doc()?.wb, at).map_err(|error| EngineError::Other(error.to_string()))? != guarded.entered {
            return Ok(false);
        }
        let Some(axes) = ids.by_id.get(&guarded.sheet) else { return Ok(false) };
        if axes.rows != guarded.rows || axes.cols != guarded.cols {
            return Ok(false);
        }
    }
    Ok(true)
}

fn restore_whole_state(session: &mut Session, ids: &WorkbookIds, state: &GuardedWholeState) -> Result<(), EngineError> {
    crate::cmd::edit(session, |cx| {
        cx.wb.names = state.names.clone();
        cx.structural = true;
        Ok(())
    })?;
    for guarded in &state.sheets {
        let at = ids.resolve_sheet(guarded.sheet).map_err(|error| EngineError::Other(target_error(error).to_string()))?;
        restore_sheet_snapshot(session, ids, at, &guarded.snapshot)?;
    }
    Ok(())
}

fn restore_sheet_snapshot(session: &mut Session, ids: &WorkbookIds, at: usize, snapshot: &GuardedSheetSnapshot) -> Result<(), EngineError> {
    crate::cmd::edit(session, |cx| {
        let target = cx.wb.sheets.get_mut(at).ok_or_else(|| EngineError::Other(format!("sheet position {at} is absent")))?;
        let mut sheet = snapshot.sheet.clone();
        sheet.file_id = snapshot.file_id;
        *target = Arc::new(sheet);
        cx.changed.extend(snapshot.sheet.cells.iter().map(|(cell, _)| (at, cell)));
        cx.structural = true;
        Ok(())
    })?;
    let mut resolved = Vec::with_capacity(snapshot.formulas.len());
    for formula in &snapshot.formulas {
        let (sheet, cell, input) = resolve_cell_input(ids, session, formula.target, &formula.input.input, true, &formula.input.references)
            .map_err(|error| EngineError::Other(target_error(error).to_string()))?;
        resolved.push((sheet, cell, input));
    }
    crate::cmd::edit(session, |cx| {
        for (sheet, cell, input) in &resolved {
            let style = cx.wb.sheet(*sheet).map(|target| target.style_id(*cell)).unwrap_or_default();
            let replacement = guarded_input_to_cell(input, true, style, &mut cx.wb)?;
            if let Some(replacement) = replacement {
                cx.sheet_mut(*sheet)?.cells.set(*cell, replacement);
            } else {
                cx.sheet_mut(*sheet)?.cells.remove(*cell);
            }
            cx.changed.push((*sheet, *cell));
        }
        Ok(())
    })?;
    Ok(())
}

fn resolve_sheet_position(ids: &WorkbookIds, position: IdentityPosition) -> Result<usize, EngineError> {
    let at = match position {
        IdentityPosition::End => ids.sheets.len(),
        IdentityPosition::Before(id) => match ids.sheets.find(id) {
            Lookup::At(at) => at,
            Lookup::Deleted { .. } => return Err(EngineError::Other(format!("sheet anchor {id:?} was deleted"))),
            Lookup::Unknown => return Err(EngineError::Other(format!("unknown sheet anchor {id:?}"))),
        },
    };
    usize::try_from(at).map_err(|_| EngineError::Other("sheet position is too large".into()))
}

fn resolve_style_target(ids: &WorkbookIds, target: StyleTarget) -> Result<ResolvedStyleTarget, TargetGone> {
    match target {
        StyleTarget::Cell { cell } => ids.resolve_cell(cell).map(|(sheet, cell)| ResolvedStyleTarget::Cell { sheet, cell }),
        StyleTarget::Row { sheet, row } => {
            let sheet_index = ids.resolve_sheet(sheet)?;
            let axes = ids.by_id.get(&sheet).ok_or(TargetGone::UnknownSheet(sheet))?;
            match axes.rows.find(row) {
                Lookup::At(row) => Ok(ResolvedStyleTarget::Row { sheet: sheet_index, row }),
                Lookup::Deleted { .. } => Err(TargetGone::DeletedRow(row)),
                Lookup::Unknown => Err(TargetGone::UnknownRow(row)),
            }
        }
        StyleTarget::Column { sheet, col } => {
            let sheet_index = ids.resolve_sheet(sheet)?;
            let axes = ids.by_id.get(&sheet).ok_or(TargetGone::UnknownSheet(sheet))?;
            match axes.cols.find(col) {
                Lookup::At(col) => Ok(ResolvedStyleTarget::Column { sheet: sheet_index, col }),
                Lookup::Deleted { .. } => Err(TargetGone::DeletedColumn(col)),
                Lookup::Unknown => Err(TargetGone::UnknownColumn(col)),
            }
        }
    }
}

fn resolved_style(wb: &Workbook, target: ResolvedStyleTarget) -> Result<&Style, EngineError> {
    let (sheet, style) = match target {
        ResolvedStyleTarget::Cell { sheet, cell } => (sheet, wb.sheet(sheet).map(|sheet| sheet.style_id(cell)).unwrap_or_default()),
        ResolvedStyleTarget::Row { sheet, row } => {
            (sheet, wb.sheet(sheet).and_then(|sheet| sheet.rows.get(&row)).and_then(|line| line.style).unwrap_or_default())
        }
        ResolvedStyleTarget::Column { sheet, col } => {
            (sheet, wb.sheet(sheet).and_then(|sheet| sheet.cols.get(&col)).and_then(|line| line.style).unwrap_or_default())
        }
    };
    if wb.sheet(sheet).is_none() {
        return Err(EngineError::Other("guarded style sheet is not available".into()));
    }
    Ok(wb.styles.get(style))
}

fn set_resolved_style(cx: &mut crate::cmd::Ctx<'_>, target: ResolvedStyleTarget, style: StyleId) -> Result<(), EngineError> {
    match target {
        ResolvedStyleTarget::Cell { sheet, cell } => {
            cx.sheet_mut(sheet)?.set_style(cell, style);
        }
        ResolvedStyleTarget::Row { sheet, row } => {
            cx.sheet_mut(sheet)?.rows.entry(row).or_default().style = Some(style);
        }
        ResolvedStyleTarget::Column { sheet, col } => {
            cx.sheet_mut(sheet)?.cols.entry(col).or_default().style = Some(style);
        }
    }
    Ok(())
}

fn json_leaf<'a>(value: &'a Json, path: &[String]) -> Option<&'a Json> {
    let mut current = value;
    for part in path {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

fn set_json_leaf(value: &mut Json, path: &[String], replacement: Json) -> Result<(), EngineError> {
    let Some((last, parents)) = path.split_last() else { return Err(EngineError::Other("guarded style path is empty".into())) };
    let mut current = value;
    for part in parents {
        current = current
            .as_object_mut()
            .and_then(|object| object.get_mut(part))
            .ok_or_else(|| EngineError::Other("guarded style path is not valid".into()))?;
    }
    let object = current.as_object_mut().ok_or_else(|| EngineError::Other("guarded style path is not valid".into()))?;
    if !object.contains_key(last) {
        return Err(EngineError::Other("guarded style path is not valid".into()));
    }
    object.insert(last.clone(), replacement);
    Ok(())
}
