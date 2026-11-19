use std::collections::{BTreeSet, VecDeque};

use collab_live::ids::{LiveId, Lookup};
use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Formula, Style, StyleId, Workbook};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use thiserror::Error;

use super::command::{StylePatch, encode_formula_in_workbook, resolve_cell_input};
use super::{CellId, CoauthorError, IdentityReference, SharedCommand, TargetGone, WorkbookIds};
use crate::{EngineError, Session};

pub const PERSONAL_HISTORY_ACTIONS: usize = 100;
pub const PERSONAL_HISTORY_BYTES: usize = 128 * 1024 * 1024;

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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
#[serde(rename_all = "camelCase")]
pub enum GuardedStructural {
    Reserved,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum GuardedPatch {
    Cells(Vec<GuardedCellContent>),
    Styles(Vec<GuardedStyleLeaf>),
    Composite(Vec<GuardedPatch>),
    Structural(GuardedStructural),
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
            Self::Structural(GuardedStructural::Reserved) => Self::Structural(GuardedStructural::Reserved),
        }
    }

    fn field_count(&self) -> usize {
        match self {
            Self::Cells(values) => values.len(),
            Self::Styles(values) => values.len(),
            Self::Composite(values) => values.iter().fold(0usize, |count, value| count.saturating_add(value.field_count())),
            Self::Structural(GuardedStructural::Reserved) => 0,
        }
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
    ids: &WorkbookIds,
    before: &Workbook,
    after: &Workbook,
    command: &SharedCommand,
) -> Result<Option<UndoAction>, CoauthorError> {
    let Some(sheet_id) = command_sheet(command) else { return Ok(None) };
    let sheet = ids.resolve_sheet(sheet_id).map_err(target_error)?;
    let before_sheet = before.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let after_sheet = after.sheet(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
    let patch = if is_content(command) {
        let mut values = Vec::new();
        for cell in before_sheet.cells.diff(&after_sheet.cells) {
            let expected = content_input(ids, after, sheet, cell)?;
            let restore = content_input(ids, before, sheet, cell)?;
            if expected != restore {
                values.push(GuardedValue { target: ids.cell_id(sheet, cell)?, expected, restore });
            }
        }
        GuardedPatch::Cells(values)
    } else if is_format(command) {
        GuardedPatch::Styles(style_changes(ids, before, after, sheet, sheet_id)?)
    } else {
        return Ok(None);
    };
    if patch.field_count() == 0 {
        return Ok(None);
    }
    let redo = patch.reversed();
    let retained_bytes = serde_json::to_vec(&patch)
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?
        .len()
        .saturating_add(serde_json::to_vec(&redo).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?.len());
    Ok(Some(UndoAction { action, label: command_label(command).to_string(), undo: patch, redo, retained_bytes }))
}

fn command_label(command: &SharedCommand) -> &'static str {
    if is_content(command) { "Cell content" } else { "Cell format" }
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

#[derive(Clone, Copy, Debug)]
enum ResolvedStyleTarget {
    Cell { sheet: usize, cell: CellRef },
    Row { sheet: usize, row: u32 },
    Column { sheet: usize, col: u32 },
}

pub(crate) fn apply_guarded_patch(session: &mut Session, ids: &WorkbookIds, patch: &GuardedPatch) -> Result<UndoReport, EngineError> {
    let mut guards = Vec::new();
    let mut left = 0usize;
    resolve_guards(ids, session, patch, &mut guards, &mut left)?;
    let restored = crate::cmd::edit(session, |cx| {
        let mut restored = 0usize;
        for guard in &guards {
            match guard {
                ResolvedGuard::CellContent { sheet, cell, expected, expected_formula, restore, restore_formula } => {
                    let current = cx.wb.sheet(*sheet).and_then(|sheet| sheet.cell(*cell));
                    let current_formula = current.is_some_and(|cell| cell.formula.is_some());
                    let current_input = current.filter(|cell| cell.py.is_none()).map(stable_content_input).unwrap_or_default();
                    if current_formula != *expected_formula || &current_input != expected {
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
                    cx.changed.push((*sheet, *cell));
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
                    restored = restored.saturating_add(1);
                }
            }
        }
        Ok(restored)
    })?;
    let reason = (left > 0).then(|| "Some fields were left because they changed after this action.".to_string());
    Ok(UndoReport { restored, left, reason })
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
        GuardedPatch::Structural(GuardedStructural::Reserved) => {
            return Err(EngineError::Other("guarded structural Undo is not available in this format revision".into()));
        }
    }
    Ok(())
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
            cx.changed.push((sheet, cell));
            cx.fit_rows.push((sheet, cell.row));
        }
        ResolvedStyleTarget::Row { sheet, row } => {
            let cells: Vec<CellRef> = cx
                .wb
                .sheet(sheet)
                .map(|sheet| sheet.cells.row(row, 0, gridcraft_core::MAX_COLS - 1).map(|(col, _)| CellRef::new(row, col)).collect())
                .unwrap_or_default();
            cx.sheet_mut(sheet)?.rows.entry(row).or_default().style = Some(style);
            cx.changed.extend(cells.iter().map(|cell| (sheet, *cell)));
            cx.fit_rows.push((sheet, row));
        }
        ResolvedStyleTarget::Column { sheet, col } => {
            let cells: Vec<CellRef> = cx
                .wb
                .sheet(sheet)
                .map(|sheet| sheet.cells.iter().filter(|(cell, _)| cell.col == col).map(|(cell, _)| cell).collect())
                .unwrap_or_default();
            cx.sheet_mut(sheet)?.cols.entry(col).or_default().style = Some(style);
            cx.changed.extend(cells.iter().map(|cell| (sheet, *cell)));
            cx.fit_rows.extend(cells.iter().map(|cell| (sheet, cell.row)));
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
