use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use collab_live::ids::{LiveId, Lookup};
use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, Formula, LineInfo, Style, StyleId, Workbook};
use protocol::live::MAX_PAYLOAD_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::command::{StylePatch, encode_formula_in_workbook, resolve_cell_input};
use super::ids::{IdentityRun, compress_runs, live_block_position, tail_runs, validate_runs};
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

/// The settings of one row or column apart from its style. The style travels separately, as
/// typed style leaves through the protection-checked style path, so no style index is shared.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedLineFields {
    pub size: Option<f32>,
    pub hidden: bool,
    pub outline: u8,
    pub collapsed: bool,
    pub custom: bool,
}

impl GuardedLineFields {
    fn of(info: Option<&LineInfo>) -> Self {
        info.map_or_else(Self::default, |info| Self {
            size: info.size,
            hidden: info.hidden,
            outline: info.outline,
            collapsed: info.collapsed,
            custom: info.custom,
        })
    }

    fn write_to(self, info: &mut LineInfo) {
        info.size = self.size;
        info.hidden = self.hidden;
        info.outline = self.outline;
        info.collapsed = self.collapsed;
        info.custom = self.custom;
    }

    /// The ranges the ordinary row-height, column-width and outline commands accept.
    fn is_valid(self) -> bool {
        self.size.is_none_or(|size| size.is_finite() && (0.0..=MAX_LINE_SIZE).contains(&size)) && self.outline <= MAX_OUTLINE_LEVEL
    }
}

/// Guard digests travel as 64 lowercase hex characters, so a patch's size does not depend on
/// the digest's bytes.
mod digest_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(digest: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(64);
        for byte in digest {
            text.push_str(&format!("{byte:02x}"));
        }
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(deserializer)?;
        let bytes = text.as_bytes();
        if bytes.len() != 64 || !bytes.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) {
            return Err(serde::de::Error::custom("a guard digest is 64 lowercase hex characters"));
        }
        let mut digest = [0u8; 32];
        for (slot, pair) in digest.iter_mut().zip(bytes.chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(serde::de::Error::custom)?;
            *slot = u8::from_str_radix(pair, 16).map_err(serde::de::Error::custom)?;
        }
        Ok(digest)
    }
}

/// Column widths are capped at 1,800 pixels; row heights (409.5 points) are below that.
const MAX_LINE_SIZE: f32 = 1800.0;
const MAX_OUTLINE_LEVEL: u8 = 7;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedLineState {
    pub target: StyleTarget,
    pub expected: GuardedLineFields,
    pub restore: GuardedLineFields,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardedAxisState {
    pub sheet: LiveId,
    pub before: IdentityPosition,
    /// The identities the original action inserted or deleted, as runs in position order.
    pub affected: Vec<IdentityRun>,
    /// The identities at the end of the axis that the original action pushed off or appended.
    pub fillers: Vec<IdentityRun>,
    pub expected_present: bool,
    #[serde(with = "digest_hex")]
    pub entered: [u8; 32],
    #[serde(with = "digest_hex")]
    pub filler_entered: [u8; 32],
    pub cells: Vec<GuardedCellContent>,
    pub styles: Vec<GuardedStyleLeaf>,
    pub lines: Vec<GuardedLineState>,
    /// Python cells the reverse cannot restore. They are left alone and reported.
    pub python_left: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum GuardedStructural {
    InsertRows { state: GuardedAxisState },
    DeleteRows { state: GuardedAxisState },
    InsertColumns { state: GuardedAxisState },
    DeleteColumns { state: GuardedAxisState },
    RenameSheet { sheet: LiveId, expected: String, restore: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum GuardedPatch {
    Cells(Vec<GuardedCellContent>),
    Styles(Vec<GuardedStyleLeaf>),
    Composite(Vec<GuardedPatch>),
    Structural(Box<GuardedStructural>),
    #[serde(skip)]
    LocalRefusal(String),
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
            Self::LocalRefusal(reason) => Self::LocalRefusal(reason.clone()),
        }
    }

    fn field_count(&self) -> usize {
        match self {
            Self::Cells(values) => values.len(),
            Self::Styles(values) => values.len(),
            Self::Composite(values) => values.iter().fold(0usize, |count, value| count.saturating_add(value.field_count())),
            Self::Structural(_) => 1,
            Self::LocalRefusal(_) => 0,
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
            Self::RenameSheet { sheet, expected, restore } => {
                Self::RenameSheet { sheet: *sheet, expected: restore.clone(), restore: expected.clone() }
            }
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
        python_left: state.python_left,
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
    #[error("{0}")]
    Refused(String),
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

    /// The labels of the actions Undo would reverse, the most recent first.
    pub fn undo_labels(&self) -> Vec<String> {
        self.undo.iter().rev().map(|action| action.label.clone()).collect()
    }

    /// Whether an Undo or Redo waits for the server; the next one must wait for it.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn prepare_undo(&mut self) -> Result<GuardedPatch, HistoryError> {
        if self.pending.is_some() {
            return Err(HistoryError::Pending);
        }
        let action = self.undo.back().ok_or(HistoryError::NothingToUndo)?;
        if let GuardedPatch::LocalRefusal(reason) = &action.undo {
            return Err(HistoryError::Refused(reason.clone()));
        }
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

const SHEET_CHANGE_REFUSAL: &str = "This sheet change cannot be undone while others are editing.";
const UNRECORDED_REFUSAL: &str = "This change cannot be undone while others are editing.";

pub(crate) fn build_undo_action(
    action: u64,
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    command: &SharedCommand,
) -> Result<Option<UndoAction>, CoauthorError> {
    // Shipping whole sheets in a shared entry is not safe, so adding and deleting sheets is
    // refused locally, before anything is sent. The marker keeps older actions below it.
    if matches!(command, SharedCommand::InsertSheet { .. } | SharedCommand::DeleteSheet { .. }) {
        return Ok(Some(refusal_action(action, command, SHEET_CHANGE_REFUSAL)));
    }
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
            // Python cells never enter a guarded patch; the rest of the change stays undoable.
            if is_python(before, sheet, cell) || is_python(after, sheet, cell) {
                continue;
            }
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

/// A history entry that refuses Undo with `reason`. It is recorded for an action whose
/// reverse cannot be shared, so Undo never falls through to an older action instead.
pub(crate) fn refusal_action(action: u64, command: &SharedCommand, reason: &str) -> UndoAction {
    let patch = GuardedPatch::LocalRefusal(reason.to_string());
    UndoAction { action, label: command_label(command).to_string(), redo: patch.clone(), undo: patch, retained_bytes: 0 }
}

/// The marker recorded when building an action's reverse fails.
pub(crate) fn unrecorded_action(action: u64, command: &SharedCommand) -> UndoAction {
    refusal_action(action, command, UNRECORDED_REFUSAL)
}

fn is_python(wb: &Workbook, sheet: usize, at: CellRef) -> bool {
    wb.sheet(sheet).and_then(|sheet| sheet.cell(at)).is_some_and(|cell| cell.py.is_some())
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
            let affected = vec![IdentityRun { first: rows.first, count: rows.count }];
            let axes = before_ids.by_id.get(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let fillers = tail_runs(&axes.rows, rows.count)?;
            GuardedStructural::InsertRows {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, rows.before, affected, fillers, true, true)?,
            }
        }
        SharedCommand::DeleteRows { sheet, rows } => {
            let count = u32::try_from(rows.ids.len()).map_err(|_| CoauthorError::MalformedPayload("too many deleted row identities".into()))?;
            let fillers = vec![IdentityRun { first: rows.first, count }];
            let (before_position, affected) = deleted_block(before_ids, *sheet, &rows.ids, true)?;
            GuardedStructural::DeleteRows {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, before_position, affected, fillers, false, true)?,
            }
        }
        SharedCommand::InsertColumns { sheet, cols } => {
            let affected = vec![IdentityRun { first: cols.first, count: cols.count }];
            let axes = before_ids.by_id.get(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let fillers = tail_runs(&axes.cols, cols.count)?;
            GuardedStructural::InsertColumns {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, cols.before, affected, fillers, true, false)?,
            }
        }
        SharedCommand::DeleteColumns { sheet, cols } => {
            let count = u32::try_from(cols.ids.len()).map_err(|_| CoauthorError::MalformedPayload("too many deleted column identities".into()))?;
            let fillers = vec![IdentityRun { first: cols.first, count }];
            let (before_position, affected) = deleted_block(before_ids, *sheet, &cols.ids, false)?;
            GuardedStructural::DeleteColumns {
                state: build_axis_state(before_ids, after_ids, before, after, *sheet, before_position, affected, fillers, false, false)?,
            }
        }
        SharedCommand::RenameSheet { sheet, .. } => {
            let before_sheet = before_ids.resolve_sheet(*sheet).map_err(target_error)?;
            let after_sheet = after_ids.resolve_sheet(*sheet).map_err(target_error)?;
            let restore = before.sheet(before_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?.name.clone();
            let expected = after.sheet(after_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?.name.clone();
            GuardedStructural::RenameSheet { sheet: *sheet, expected, restore }
        }
        _ => return Ok(None),
    };
    Ok(Some(structural))
}

/// The anchor just below a deleted block, and the block's identities as runs in position
/// order. A block that reached the end of the axis anchors at `End`.
fn deleted_block(ids: &WorkbookIds, sheet: LiveId, targets: &[LiveId], rows: bool) -> Result<(IdentityPosition, Vec<IdentityRun>), CoauthorError> {
    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let ordered = if rows { &axes.rows } else { &axes.cols };
    let mut sorted = targets.to_vec();
    sorted.sort_unstable();
    let runs = compress_runs(sorted);
    let first = live_block_position(ordered, &runs)?
        .ok_or_else(|| CoauthorError::MalformedPayload("structural deletion identities are not consecutive".into()))?;
    let count = u32::try_from(targets.len()).map_err(|_| CoauthorError::MalformedPayload("too many structural identities".into()))?;
    let after = first.checked_add(count).ok_or_else(|| CoauthorError::MalformedPayload("structural deletion position overflows".into()))?;
    let mut positioned = Vec::with_capacity(runs.len());
    for run in runs {
        let at = live_block_position(ordered, std::slice::from_ref(&run))?
            .ok_or_else(|| CoauthorError::MalformedPayload("structural deletion identities are not live".into()))?;
        positioned.push((at, run));
    }
    positioned.sort_unstable();
    let affected =
        compress_runs(positioned.into_iter().flat_map(|(_, run)| {
            (0..run.count).filter_map(move |offset| run.first.1.checked_add(offset).map(|counter| LiveId(run.first.0, counter)))
        }));
    Ok((ordered.id_at(after).map_or(IdentityPosition::End, IdentityPosition::Before), affected))
}

#[allow(clippy::too_many_arguments)]
fn build_axis_state(
    before_ids: &WorkbookIds,
    after_ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    sheet: LiveId,
    position: IdentityPosition,
    affected: Vec<IdentityRun>,
    fillers: Vec<IdentityRun>,
    expected_present: bool,
    rows: bool,
) -> Result<GuardedAxisState, CoauthorError> {
    let changes = structural_changes(before_ids, after_ids, before, after)?;
    let (present_ids, present_wb) = if expected_present { (after_ids, after) } else { (before_ids, before) };
    let entered = axis_entered(present_ids, present_wb, sheet, &affected, rows)?
        .ok_or_else(|| CoauthorError::MalformedPayload("structural identities are not one live block".into()))?;
    let (filler_ids, filler_wb) = if expected_present { (before_ids, before) } else { (after_ids, after) };
    let filler_entered = axis_entered(filler_ids, filler_wb, sheet, &fillers, rows)?
        .ok_or_else(|| CoauthorError::MalformedPayload("structural tail identities are not one live block".into()))?;
    Ok(GuardedAxisState {
        sheet,
        before: position,
        affected,
        fillers,
        expected_present,
        entered,
        filler_entered,
        cells: changes.cells,
        styles: changes.styles,
        lines: changes.lines,
        python_left: changes.python_left,
    })
}

struct StructuralChanges {
    cells: Vec<GuardedCellContent>,
    styles: Vec<GuardedStyleLeaf>,
    lines: Vec<GuardedLineState>,
    python_left: u32,
}

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
    let mut python_left = 0u32;
    let mut skipped = BTreeSet::new();
    for target in &targets {
        let restore = before_cells.get(target);
        let expected = after_cells.get(target);
        // A Python cell is never shipped. One that appears on only one side is left alone and
        // counted for the undo report; one on both sides did not change identity.
        let python_before = matches!(restore, Some(None));
        let python_after = matches!(expected, Some(None));
        if python_before || python_after {
            if python_before != python_after {
                python_left = python_left.saturating_add(1);
            }
            skipped.insert(*target);
            continue;
        }
        let restore = restore.and_then(Option::as_ref).unwrap_or(&empty);
        let expected = expected.and_then(Option::as_ref).unwrap_or(&empty);
        if restore != expected {
            cells.push(GuardedValue { target: *target, expected: expected.clone(), restore: restore.clone() });
        }
    }

    let before_styles = cell_styles(before_ids, before)?;
    let after_styles = cell_styles(after_ids, after)?;
    let default_style = style_json(&Style::default())?;
    let mut styles = Vec::new();
    for target in targets.difference(&skipped) {
        append_style_leaves(
            StyleTarget::Cell { cell: *target },
            before_styles.get(target).cloned().unwrap_or_else(|| default_style.clone()),
            after_styles.get(target).cloned().unwrap_or_else(|| default_style.clone()),
            &mut styles,
        )?;
    }

    let before_lines = line_states(before_ids, before)?;
    let after_lines = line_states(after_ids, after)?;
    let line_targets: BTreeSet<StyleTarget> = before_lines.keys().chain(after_lines.keys()).copied().collect();
    let mut lines = Vec::new();
    for target in line_targets {
        let restore = before_lines.get(&target);
        let expected = after_lines.get(&target);
        let (restore_fields, expected_fields) =
            (GuardedLineFields::of(restore.map(|(info, _)| info)), GuardedLineFields::of(expected.map(|(info, _)| info)));
        if restore_fields != expected_fields {
            lines.push(GuardedLineState { target, expected: expected_fields, restore: restore_fields });
        }
        append_style_leaves(
            target,
            restore.map_or_else(|| default_style.clone(), |(_, style)| style.clone()),
            expected.map_or_else(|| default_style.clone(), |(_, style)| style.clone()),
            &mut styles,
        )?;
    }
    Ok(StructuralChanges { cells, styles, lines, python_left })
}

/// Every stored cell's typed input by identity; `None` marks a Python cell, which is never
/// encoded.
fn cell_inputs(ids: &WorkbookIds, wb: &Workbook) -> Result<BTreeMap<CellId, Option<GuardedCellInput>>, CoauthorError> {
    let mut values = BTreeMap::new();
    for (sheet, target_sheet) in wb.sheets.iter().enumerate() {
        for (cell, value) in target_sheet.cells.iter() {
            let input = if value.py.is_some() { None } else { Some(content_input(ids, wb, sheet, cell)?) };
            values.insert(ids.cell_id(sheet, cell)?, input);
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

/// Every row and column setting by identity, with its style resolved to typed JSON.
fn line_states(ids: &WorkbookIds, wb: &Workbook) -> Result<BTreeMap<StyleTarget, (LineInfo, Json)>, CoauthorError> {
    let mut values = BTreeMap::new();
    for (sheet, target_sheet) in wb.sheets.iter().enumerate() {
        let sheet_id = ids.sheet_id(sheet)?;
        let axes = ids.by_id.get(&sheet_id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
        for (row, info) in &target_sheet.rows {
            let row = axes.rows.id_at(*row).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row position {row}")))?;
            values.insert(StyleTarget::Row { sheet: sheet_id, row }, (*info, style_json(wb.styles.get(info.style.unwrap_or_default()))?));
        }
        for (col, info) in &target_sheet.cols {
            let col = axes.cols.id_at(*col).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column position {col}")))?;
            values.insert(StyleTarget::Column { sheet: sheet_id, col }, (*info, style_json(wb.styles.get(info.style.unwrap_or_default()))?));
        }
    }
    Ok(values)
}

/// A digest of everything entered in the live block `runs` on one axis: cell inputs, cell
/// styles and line settings, all by identity and typed value. Style indices and Python source
/// are never hashed as such, so every client computes the same digest. `None` when the runs are
/// not one live block.
fn axis_entered(ids: &WorkbookIds, wb: &Workbook, sheet: LiveId, runs: &[IdentityRun], rows: bool) -> Result<Option<[u8; 32]>, CoauthorError> {
    let sheet_index = ids.resolve_sheet(sheet).map_err(target_error)?;
    let target_sheet = wb.sheet(sheet_index).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
    let ordered = if rows { &axes.rows } else { &axes.cols };
    let count = validate_runs(runs, u32::MAX)?;
    let Some(start) = live_block_position(ordered, runs)? else { return Ok(None) };
    let end = start.saturating_add(count);
    let mut cells = Vec::new();
    for (cell, value) in target_sheet.cells.iter() {
        let at = if rows { cell.row } else { cell.col };
        if at < start || at >= end {
            continue;
        }
        let input = if let Some(python) = value.py.as_ref() {
            GuardedCellInput { input: format!("python:{}", python.code), formula: true, references: Vec::new() }
        } else {
            content_input(ids, wb, sheet_index, cell)?
        };
        cells.push((ids.cell_id(sheet_index, cell)?, input, style_json(wb.styles.get(target_sheet.style_id(cell)))?));
    }
    let lines_map = if rows { &target_sheet.rows } else { &target_sheet.cols };
    let mut lines = Vec::new();
    for (at, info) in lines_map.range(start..end) {
        let id = ordered.id_at(*at).ok_or_else(|| CoauthorError::UnknownIdentity(format!("axis position {at}")))?;
        lines.push((id, GuardedLineFields::of(Some(info)), style_json(wb.styles.get(info.style.unwrap_or_default()))?));
    }
    let bytes = serde_json::to_vec(&(runs, cells, lines)).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    let mut entered = [0u8; 32];
    entered.copy_from_slice(&digest);
    Ok(Some(entered))
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
        TargetGone::TooFragmented => CoauthorError::TooFragmented,
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

pub(crate) fn apply_guarded_patch(
    session: &mut Session,
    ids: &mut WorkbookIds,
    patch: &GuardedPatch,
    participant: u32,
) -> Result<UndoReport, EngineError> {
    validate_guarded_patch(patch)?;
    if let GuardedPatch::Structural(structural) = patch {
        return apply_structural_patch(session, ids, structural, participant);
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
        GuardedPatch::LocalRefusal(_) => return Err(EngineError::Other("a local undo refusal cannot enter a shared patch".into())),
    }
    Ok(())
}

fn validate_structural_guards(structural: &GuardedStructural, validator: &mut GuardValidator) -> Result<(), EngineError> {
    match structural {
        GuardedStructural::InsertRows { state } | GuardedStructural::DeleteRows { state } => validate_axis_state(state, true, validator)?,
        GuardedStructural::InsertColumns { state } | GuardedStructural::DeleteColumns { state } => validate_axis_state(state, false, validator)?,
        GuardedStructural::RenameSheet { .. } => {
            validator.count_items(1)?;
        }
    }
    Ok(())
}

/// Bounds a structural guard by runs (contiguous identity ranges), so a large contiguous undo
/// is cheap, and limits row and column settings to the identities the entry revives.
fn validate_axis_state(state: &GuardedAxisState, rows: bool, validator: &mut GuardValidator) -> Result<(), EngineError> {
    validator.count_items(state.affected.len().saturating_add(state.fillers.len()))?;
    let limit = if rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
    let affected = validate_runs(&state.affected, limit).map_err(|error| EngineError::Other(error.to_string()))?;
    let fillers = validate_runs(&state.fillers, limit).map_err(|error| EngineError::Other(error.to_string()))?;
    if affected != fillers {
        return Err(EngineError::Other("structural identity exchange is not balanced".into()));
    }
    let mut revived = state.affected.clone();
    revived.sort_unstable();
    validate_cell_guards(&state.cells, validator)?;
    validate_style_guards(&state.styles, validator)?;
    for style in &state.styles {
        if !matches!(style.target, StyleTarget::Cell { .. }) && !line_target_is_revived(style.target, state.sheet, rows, &revived) {
            return Err(EngineError::Other("a structural style guard names a line the entry does not restore".into()));
        }
    }
    for line in &state.lines {
        validator.add(GuardKey::Line(line.target))?;
        if !line_target_is_revived(line.target, state.sheet, rows, &revived) {
            return Err(EngineError::Other("a structural line guard names a line the entry does not restore".into()));
        }
        if !line.expected.is_valid() || !line.restore.is_valid() {
            return Err(EngineError::Other("a structural line guard is out of range".into()));
        }
    }
    Ok(())
}

/// Whether `target` is a row (or column) of `sheet` among the sorted runs `revived`.
fn line_target_is_revived(target: StyleTarget, sheet: LiveId, rows: bool, revived: &[IdentityRun]) -> bool {
    let id = match target {
        StyleTarget::Row { sheet: target_sheet, row } if rows && target_sheet == sheet => row,
        StyleTarget::Column { sheet: target_sheet, col } if !rows && target_sheet == sheet => col,
        _ => return false,
    };
    let index = revived.partition_point(|run| (run.first.0, run.first.1) <= (id.0, id.1));
    index.checked_sub(1).and_then(|index| revived.get(index)).is_some_and(|run| run.holds(id))
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
        GuardedPatch::LocalRefusal(_) => return Err(EngineError::Other("a local undo refusal cannot enter a shared patch".into())),
    }
    Ok(())
}

fn apply_structural_patch(
    session: &mut Session,
    ids: &mut WorkbookIds,
    structural: &GuardedStructural,
    participant: u32,
) -> Result<UndoReport, EngineError> {
    match structural {
        GuardedStructural::InsertRows { state } | GuardedStructural::DeleteRows { state } => apply_axis_patch(session, ids, state, true, participant),
        GuardedStructural::InsertColumns { state } | GuardedStructural::DeleteColumns { state } => {
            apply_axis_patch(session, ids, state, false, participant)
        }
        GuardedStructural::RenameSheet { sheet, expected, restore } => apply_rename_sheet(session, ids, *sheet, expected, restore),
    }
}

fn apply_axis_patch(
    session: &mut Session,
    ids: &mut WorkbookIds,
    state: &GuardedAxisState,
    rows: bool,
    participant: u32,
) -> Result<UndoReport, EngineError> {
    let limit = if rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
    let count = validate_runs(&state.affected, limit).map_err(|error| EngineError::Other(error.to_string()))?;
    let label = if rows { "rows" } else { "columns" };
    let sheet_index = ids.resolve_sheet(state.sheet).map_err(|error| EngineError::Other(target_error(error).to_string()))?;
    // A shared handle, not a copy: the exchange below copies the sheet's tables once anyway.
    let axes = Arc::clone(ids.by_id.get(&state.sheet).ok_or_else(|| EngineError::Other(format!("unknown sheet identity {:?}", state.sheet)))?);
    let ordered = if rows { &axes.rows } else { &axes.cols };
    let engine = |error: CoauthorError| EngineError::Other(error.to_string());
    // The identities this entry revives must have been deleted by this entry's participant,
    // according to the ordered entries this client has applied. A forged revival of someone
    // else's deletion is refused before anything changes.
    let revived = if state.expected_present { &state.fillers } else { &state.affected };
    if !ids.deleted_by_participant(state.sheet, rows, revived, participant) {
        return Err(EngineError::Other(format!("the {label} to restore were not deleted by this participant")));
    }
    if state.expected_present {
        let Some(at) = live_block_position(ordered, &state.affected).map_err(engine)? else { return Ok(untouched_refusal(label)) };
        if axis_entered(ids, &session.doc()?.wb, state.sheet, &state.affected, rows).map_err(engine)? != Some(state.entered) {
            return Ok(untouched_refusal(label));
        }
        let command = if rows { "home.deleteRows" } else { "home.deleteColumns" };
        let range = axis_range(at, count, rows)?;
        let params = if rows { serde_json::json!({"sheet":sheet_index,"rows":range}) } else { serde_json::json!({"sheet":sheet_index,"cols":range}) };
        session.execute(command, params)?;
        let restore_at = limit.checked_sub(count).ok_or_else(|| EngineError::Other("structural identity count exceeds the worksheet".into()))?;
        ids.exchange_axis(state.sheet, rows, &state.affected, restore_at, &state.fillers, participant).map_err(engine)?;
        Ok(UndoReport { restored: 1, left: 0, reason: None })
    } else {
        let expected_tail = limit.checked_sub(count).ok_or_else(|| EngineError::Other("structural identity count exceeds the worksheet".into()))?;
        if live_block_position(ordered, &state.fillers).map_err(engine)? != Some(expected_tail)
            || axis_entered(ids, &session.doc()?.wb, state.sheet, &state.fillers, rows).map_err(engine)? != Some(state.filler_entered)
        {
            return Ok(untouched_refusal(label));
        }
        let Some(at) = resolve_axis_position(ordered, state.before, count)? else { return Ok(untouched_refusal(label)) };
        let command = if rows { "home.insertRows" } else { "home.insertColumns" };
        let range = axis_range(at, count, rows)?;
        let params = if rows { serde_json::json!({"sheet":sheet_index,"rows":range}) } else { serde_json::json!({"sheet":sheet_index,"cols":range}) };
        session.execute(command, params)?;
        ids.exchange_axis(state.sheet, rows, &state.fillers, at, &state.affected, participant).map_err(engine)?;
        let changes = GuardedPatch::Composite(vec![GuardedPatch::Cells(state.cells.clone()), GuardedPatch::Styles(state.styles.clone())]);
        let mut report = apply_guarded_patch(session, ids, &changes, participant)?;
        let lines_left = apply_line_states(session, ids, &state.lines, rows)?;
        report.left = report.left.saturating_add(lines_left);
        if lines_left > 0 && report.reason.is_none() {
            report.reason = Some("Some fields were left because they changed after this action.".into());
        }
        if state.python_left > 0 {
            let python_left = usize::try_from(state.python_left).unwrap_or(usize::MAX);
            report.left = report.left.saturating_add(python_left);
            let note = "Python cells were left alone: they cannot be restored while others are editing.";
            report.reason = Some(match report.reason.take() {
                Some(reason) => format!("{reason} {note}"),
                None => note.to_string(),
            });
        }
        report.restored = report.restored.saturating_add(1);
        Ok(report)
    }
}

/// Refuses a structural Undo or Redo that would pass the live limits on its axis. Each
/// machine checks before applying the entry, and the sender before sending it, alike from the
/// identity tables. Other faults are left to the apply, which reports them as before.
pub(crate) fn check_guarded_fragmentation(ids: &WorkbookIds, patch: &GuardedPatch) -> Result<(), TargetGone> {
    let GuardedPatch::Structural(structural) = patch else { return Ok(()) };
    let (state, rows) = match structural.as_ref() {
        GuardedStructural::InsertRows { state } | GuardedStructural::DeleteRows { state } => (state, true),
        GuardedStructural::InsertColumns { state } | GuardedStructural::DeleteColumns { state } => (state, false),
        GuardedStructural::RenameSheet { .. } => return Ok(()),
    };
    // Undoing an insert removes the inserted block and revives the fillers at the end; undoing
    // a deletion removes the fillers and revives the deleted block where it was.
    let (remove, restore, insert_at) = if state.expected_present {
        let limit = if rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
        let count = state.affected.iter().fold(0u32, |total, run| total.saturating_add(run.count));
        (&state.affected, &state.fillers, limit.saturating_sub(count))
    } else {
        (&state.fillers, &state.affected, 0)
    };
    match ids.check_exchange_fragmentation(state.sheet, rows, remove, insert_at, restore) {
        Err(CoauthorError::TooFragmented) => Err(TargetGone::TooFragmented),
        _ => Ok(()),
    }
}

fn untouched_refusal(target: &str) -> UndoReport {
    UndoReport { restored: 0, left: 1, reason: Some(format!("The {target} changed after this action; the structural change was left untouched.")) }
}

/// Where a reverted deletion goes back. A block that was deleted from the end of the axis has
/// no anchor below it; it returns just above the fillers that replaced it. `None` when the
/// anchor itself has since been deleted.
fn resolve_axis_position(ordered: &collab_live::ids::OrderedIds, position: IdentityPosition, count: u32) -> Result<Option<u32>, EngineError> {
    match position {
        IdentityPosition::End => {
            ordered.len().checked_sub(count).map(Some).ok_or_else(|| EngineError::Other("structural identity count exceeds the worksheet".into()))
        }
        IdentityPosition::Before(id) => match ordered.find(id) {
            Lookup::At(at) => Ok(Some(at)),
            Lookup::Deleted { .. } => Ok(None),
            Lookup::Unknown => Err(EngineError::Other(format!("unknown structural anchor {id:?}"))),
        },
    }
}

fn axis_range(at: u32, count: u32, rows: bool) -> Result<String, EngineError> {
    let end = at.checked_add(count).and_then(|end| end.checked_sub(1)).ok_or_else(|| EngineError::Other("structural range overflows".into()))?;
    Ok(if rows { RangeRef::rows(at, end) } else { RangeRef::cols(at, end) }.a1())
}

/// Restores typed row or column settings (size, hidden, outline) where they still hold what
/// this action left, under the sheet's format-rows or format-columns protection. Returns how
/// many were left because they changed.
fn apply_line_states(session: &mut Session, ids: &WorkbookIds, lines: &[GuardedLineState], rows: bool) -> Result<usize, EngineError> {
    let mut resolved = Vec::with_capacity(lines.len());
    for line in lines {
        match resolve_style_target(ids, line.target).map_err(|error| EngineError::Other(target_error(error).to_string()))? {
            ResolvedStyleTarget::Row { sheet, row } if rows => resolved.push((sheet, row, line)),
            ResolvedStyleTarget::Column { sheet, col } if !rows => resolved.push((sheet, col, line)),
            _ => return Err(EngineError::Other("a guarded line target names the wrong axis".into())),
        }
    }
    {
        let wb = &session.doc()?.wb;
        for (sheet, _, _) in &resolved {
            if let Some(protection) = wb.sheet(*sheet).and_then(|sheet| sheet.protection.as_ref())
                && !(if rows { protection.format_rows } else { protection.format_columns })
            {
                return Err(EngineError::Other("The row or column you're trying to change is on a protected sheet.".into()));
            }
        }
    }
    crate::cmd::edit(session, |cx| {
        let mut left = 0usize;
        for (sheet, at, line) in &resolved {
            let target = cx.sheet_mut(*sheet)?;
            let map = if rows { &mut target.rows } else { &mut target.cols };
            if GuardedLineFields::of(map.get(at)) != line.expected {
                left = left.saturating_add(1);
                continue;
            }
            let mut info = map.get(at).copied().unwrap_or_default();
            line.restore.write_to(&mut info);
            if info == LineInfo::default() {
                map.remove(at);
            } else {
                map.insert(*at, info);
            }
            if rows {
                cx.fit_rows.push((*sheet, *at));
            }
            cx.structural = true;
        }
        Ok(left)
    })
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
