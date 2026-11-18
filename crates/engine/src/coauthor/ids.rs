use std::collections::{BTreeMap, BTreeSet};

use collab_live::ids::{IdError, LiveId, Lookup, OrderedIds};
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::CoauthorError;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellId {
    pub sheet: LiveId,
    pub row: LiveId,
    pub col: LiveId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SheetIds {
    pub id: LiveId,
    pub rows: OrderedIds,
    pub cols: OrderedIds,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkbookIds {
    pub sheets: OrderedIds,
    pub by_id: BTreeMap<LiveId, SheetIds>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IdentityRangeKind {
    Cells,
    Rows,
    Columns,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityRange {
    pub sheet: LiveId,
    pub start: CellId,
    pub end: CellId,
    pub kind: IdentityRangeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum IdentityRanges {
    One(IdentityRange),
    Many(Vec<IdentityRange>),
}

impl IdentityRanges {
    pub fn from_vec(mut ranges: Vec<IdentityRange>) -> Result<Self, CoauthorError> {
        if ranges.len() == 1 {
            ranges.pop().map(Self::One).ok_or_else(|| CoauthorError::MalformedPayload("live range is absent".into()))
        } else if ranges.is_empty() {
            Err(CoauthorError::MalformedPayload("live range is absent".into()))
        } else {
            Ok(Self::Many(ranges))
        }
    }

    pub fn as_slice(&self) -> &[IdentityRange] {
        match self {
            Self::One(range) => std::slice::from_ref(range),
            Self::Many(ranges) => ranges,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IdentityPosition {
    Before(LiveId),
    End,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityInsert {
    pub before: IdentityPosition,
    pub first: LiveId,
    pub count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityDelete {
    pub ids: Vec<LiveId>,
    pub first: LiveId,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentitySheetInsert {
    pub before: IdentityPosition,
    pub id: LiveId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum IdentityStructural {
    InsertRows { sheet: LiveId, before: IdentityPosition, first: LiveId, count: u32 },
    DeleteRows { sheet: LiveId, ids: Vec<LiveId>, first: LiveId },
    InsertColumns { sheet: LiveId, before: IdentityPosition, first: LiveId, count: u32 },
    DeleteColumns { sheet: LiveId, ids: Vec<LiveId>, first: LiveId },
    InsertSheet { before: IdentityPosition, id: LiveId },
    DeleteSheet { id: LiveId },
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TargetGone {
    #[error("sheet identity {0:?} was deleted")]
    DeletedSheet(LiveId),
    #[error("row identity {0:?} was deleted")]
    DeletedRow(LiveId),
    #[error("column identity {0:?} was deleted")]
    DeletedColumn(LiveId),
    #[error("unknown sheet identity {0:?}")]
    UnknownSheet(LiveId),
    #[error("unknown row identity {0:?}")]
    UnknownRow(LiveId),
    #[error("unknown column identity {0:?}")]
    UnknownColumn(LiveId),
    #[error("malformed live target: {0}")]
    Malformed(String),
    #[error("formula reference no longer has one coordinate across its 3-D sheet span")]
    UnrepresentableReference,
}

impl WorkbookIds {
    pub fn from_workbook(wb: &Workbook) -> Result<Self, CoauthorError> {
        let sheet_count =
            u32::try_from(wb.sheets.len()).map_err(|_| CoauthorError::MalformedPayload("too many sheets for live identities".into()))?;
        let sheets = OrderedIds::base(sheet_count);
        let mut by_id = BTreeMap::new();
        for position in 0..sheet_count {
            let id = LiveId(0, position);
            by_id.insert(id, SheetIds { id, rows: OrderedIds::base(MAX_ROWS), cols: OrderedIds::base(MAX_COLS) });
        }
        Ok(Self { sheets, by_id })
    }

    pub fn cell_id(&self, sheet: usize, cell: CellRef) -> Result<CellId, CoauthorError> {
        if !cell.is_valid() {
            return Err(CoauthorError::MalformedPayload(format!("cell `{cell}` is outside the worksheet")));
        }
        let sheet_position = u32::try_from(sheet).map_err(|_| CoauthorError::UnknownIdentity(format!("sheet position {sheet}")))?;
        let sheet_id = self.sheets.id_at(sheet_position).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet position {sheet}")))?;
        let axis = self.by_id.get(&sheet_id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet_id:?}")))?;
        let row = axis.rows.id_at(cell.row).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row {}", cell.row)))?;
        let col = axis.cols.id_at(cell.col).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column {}", cell.col)))?;
        Ok(CellId { sheet: sheet_id, row, col })
    }

    pub fn range_id(&self, sheet: usize, range: RangeRef) -> Result<IdentityRange, CoauthorError> {
        let kind = if range.is_full_rows() && !range.is_full_cols() {
            IdentityRangeKind::Rows
        } else if range.is_full_cols() && !range.is_full_rows() {
            IdentityRangeKind::Columns
        } else {
            IdentityRangeKind::Cells
        };
        Ok(IdentityRange { sheet: self.sheet_id(sheet)?, start: self.cell_id(sheet, range.start)?, end: self.cell_id(sheet, range.end)?, kind })
    }

    pub fn sheet_id(&self, sheet: usize) -> Result<LiveId, CoauthorError> {
        let position = u32::try_from(sheet).map_err(|_| CoauthorError::UnknownIdentity(format!("sheet position {sheet}")))?;
        self.sheets.id_at(position).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet position {sheet}")))
    }

    pub fn resolve_sheet(&self, id: LiveId) -> Result<usize, TargetGone> {
        match self.sheets.find(id) {
            Lookup::At(position) => usize::try_from(position).map_err(|_| TargetGone::UnknownSheet(id)),
            Lookup::Deleted { .. } => Err(TargetGone::DeletedSheet(id)),
            Lookup::Unknown => Err(TargetGone::UnknownSheet(id)),
        }
    }

    pub fn resolve_cell(&self, id: CellId) -> Result<(usize, CellRef), TargetGone> {
        let sheet = self.resolve_sheet(id.sheet)?;
        let axes = self.by_id.get(&id.sheet).ok_or(TargetGone::UnknownSheet(id.sheet))?;
        let row = resolve_axis(&axes.rows, id.row, AxisIdentity::Row)?;
        let col = resolve_axis(&axes.cols, id.col, AxisIdentity::Column)?;
        Ok((sheet, CellRef::new(row, col)))
    }

    pub fn resolve_range(&self, id: IdentityRange) -> Result<(usize, RangeRef), TargetGone> {
        if id.start.sheet != id.sheet || id.end.sheet != id.sheet {
            return Err(TargetGone::Malformed("range endpoints name a different sheet".into()));
        }
        let sheet = self.resolve_sheet(id.sheet)?;
        let axes = self.by_id.get(&id.sheet).ok_or(TargetGone::UnknownSheet(id.sheet))?;
        let range = match id.kind {
            IdentityRangeKind::Cells => {
                let (_, start) = self.resolve_cell(id.start)?;
                let (_, end) = self.resolve_cell(id.end)?;
                RangeRef::new(start, end)
            }
            IdentityRangeKind::Rows => {
                let start = resolve_axis(&axes.rows, id.start.row, AxisIdentity::Row)?;
                let end = resolve_axis(&axes.rows, id.end.row, AxisIdentity::Row)?;
                RangeRef::rows(start.min(end), start.max(end))
            }
            IdentityRangeKind::Columns => {
                let start = resolve_axis(&axes.cols, id.start.col, AxisIdentity::Column)?;
                let end = resolve_axis(&axes.cols, id.end.col, AxisIdentity::Column)?;
                RangeRef::cols(start.min(end), start.max(end))
            }
        };
        Ok((sheet, range))
    }

    pub fn apply_structural(&mut self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        let mut next = self.clone();
        next.apply_structural_in_place(change)?;
        *self = next;
        Ok(())
    }

    fn apply_structural_in_place(&mut self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        match change {
            IdentityStructural::InsertRows { sheet, before, first, count } => {
                let axes = self.live_sheet_mut(*sheet)?;
                insert_fixed(&mut axes.rows, *before, *first, *count, MAX_ROWS)?;
            }
            IdentityStructural::DeleteRows { sheet, ids, first } => {
                let axes = self.live_sheet_mut(*sheet)?;
                delete_fixed(&mut axes.rows, ids, *first, "row", MAX_ROWS)?;
            }
            IdentityStructural::InsertColumns { sheet, before, first, count } => {
                let axes = self.live_sheet_mut(*sheet)?;
                insert_fixed(&mut axes.cols, *before, *first, *count, MAX_COLS)?;
            }
            IdentityStructural::DeleteColumns { sheet, ids, first } => {
                let axes = self.live_sheet_mut(*sheet)?;
                delete_fixed(&mut axes.cols, ids, *first, "column", MAX_COLS)?;
            }
            IdentityStructural::InsertSheet { before, id } => {
                let at = position(&self.sheets, *before, "sheet")?;
                self.sheets.insert(at, *id, 1)?;
                self.by_id.insert(*id, SheetIds { id: *id, rows: OrderedIds::base(MAX_ROWS), cols: OrderedIds::base(MAX_COLS) });
            }
            IdentityStructural::DeleteSheet { id } => delete_exact(&mut self.sheets, &[*id], "sheet")?,
        }
        Ok(())
    }

    fn live_sheet_mut(&mut self, id: LiveId) -> Result<&mut SheetIds, CoauthorError> {
        match self.sheets.find(id) {
            Lookup::At(_) => self.by_id.get_mut(&id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {id:?}"))),
            Lookup::Deleted { .. } => Err(CoauthorError::DeletedTarget(format!("sheet {id:?}"))),
            Lookup::Unknown => Err(CoauthorError::UnknownIdentity(format!("sheet {id:?}"))),
        }
    }
}

#[derive(Clone, Copy)]
enum AxisIdentity {
    Row,
    Column,
}

fn resolve_axis(ids: &OrderedIds, id: LiveId, axis: AxisIdentity) -> Result<u32, TargetGone> {
    match (ids.find(id), axis) {
        (Lookup::At(position), _) => Ok(position),
        (Lookup::Deleted { .. }, AxisIdentity::Row) => Err(TargetGone::DeletedRow(id)),
        (Lookup::Deleted { .. }, AxisIdentity::Column) => Err(TargetGone::DeletedColumn(id)),
        (Lookup::Unknown, AxisIdentity::Row) => Err(TargetGone::UnknownRow(id)),
        (Lookup::Unknown, AxisIdentity::Column) => Err(TargetGone::UnknownColumn(id)),
    }
}

fn position(ids: &OrderedIds, before: IdentityPosition, label: &str) -> Result<u32, CoauthorError> {
    match before {
        IdentityPosition::End => Ok(ids.len()),
        IdentityPosition::Before(id) => match ids.find(id) {
            Lookup::At(at) => Ok(at),
            Lookup::Deleted { .. } => Err(CoauthorError::DeletedTarget(format!("{label} {id:?}"))),
            Lookup::Unknown => Err(CoauthorError::UnknownIdentity(format!("{label} {id:?}"))),
        },
    }
}

fn insert_fixed(ids: &mut OrderedIds, before: IdentityPosition, first: LiveId, count: u32, limit: u32) -> Result<(), CoauthorError> {
    if count == 0 || count > limit {
        return Err(CoauthorError::MalformedPayload(format!("insert count {count} is outside 1..={limit}")));
    }
    let at = position(ids, before, "axis")?;
    if ids.len() != limit || at.checked_add(count).is_none_or(|end| end > limit) {
        return Err(CoauthorError::MalformedPayload("inserted identities are outside the worksheet".into()));
    }
    ids.insert(at, first, count)?;
    ids.delete(limit, count)?;
    Ok(())
}

fn delete_fixed(ids: &mut OrderedIds, targets: &[LiveId], first: LiveId, label: &str, limit: u32) -> Result<(), CoauthorError> {
    if targets.is_empty() || ids.len() != limit {
        return Err(CoauthorError::MalformedPayload(format!("{label} deletion must keep a full worksheet axis")));
    }
    delete_exact(ids, targets, label)?;
    let count = u32::try_from(targets.len()).map_err(|_| CoauthorError::MalformedPayload(format!("too many {label} identities")))?;
    ids.insert(ids.len(), first, count)?;
    Ok(())
}

fn delete_exact(ids: &mut OrderedIds, targets: &[LiveId], label: &str) -> Result<(), CoauthorError> {
    let mut positions = Vec::with_capacity(targets.len());
    let mut unique = BTreeSet::new();
    for id in targets {
        if !unique.insert(*id) {
            return Err(CoauthorError::MalformedPayload(format!("duplicate {label} identity {id:?}")));
        }
        match ids.find(*id) {
            Lookup::At(at) => positions.push(at),
            Lookup::Deleted { .. } => return Err(CoauthorError::DeletedTarget(format!("{label} {id:?}"))),
            Lookup::Unknown => return Err(CoauthorError::UnknownIdentity(format!("{label} {id:?}"))),
        }
    }
    positions.sort_unstable_by(|a, b| b.cmp(a));
    for at in positions {
        ids.delete(at, 1)?;
    }
    Ok(())
}

impl From<IdError> for CoauthorError {
    fn from(value: IdError) -> Self {
        CoauthorError::Identity(value)
    }
}
