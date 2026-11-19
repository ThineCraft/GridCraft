use std::collections::{BTreeMap, BTreeSet};

use collab_live::ids::{IdError, LiveId, Lookup, OrderedIds};
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::CoauthorError;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

/// `count` consecutive identities of one participant, starting at `first`. Structural undo
/// patches carry identities in this form, so a contiguous block of any size costs one entry.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct IdentityRun {
    pub first: LiveId,
    pub count: u32,
}

impl IdentityRun {
    /// The counter one past the run's last, as `u64` so it cannot overflow.
    pub fn end(self) -> u64 {
        u64::from(self.first.1).saturating_add(u64::from(self.count))
    }

    pub fn holds(self, id: LiveId) -> bool {
        id.0 == self.first.0 && id.1 >= self.first.1 && u64::from(id.1) < self.end()
    }
}

/// Compresses identities into runs, keeping their order.
pub(crate) fn compress_runs(ids: impl IntoIterator<Item = LiveId>) -> Vec<IdentityRun> {
    let mut runs: Vec<IdentityRun> = Vec::new();
    for id in ids {
        if let Some(last) = runs.last_mut()
            && last.first.0 == id.0
            && last.end() == u64::from(id.1)
            && last.count < u32::MAX
        {
            last.count += 1;
            continue;
        }
        runs.push(IdentityRun { first: id, count: 1 });
    }
    runs
}

/// Checks that runs are non-empty, in range and disjoint, and returns how many identities they
/// name. `limit` is the most identities a structural patch may name on this axis.
pub(crate) fn validate_runs(runs: &[IdentityRun], limit: u32) -> Result<u32, CoauthorError> {
    let mut sorted: Vec<(u32, u64, u64)> = Vec::with_capacity(runs.len());
    let mut total = 0u64;
    for run in runs {
        if run.count == 0 || run.end() > u64::from(u32::MAX) + 1 {
            return Err(CoauthorError::MalformedPayload("identity run is empty or overflows".into()));
        }
        total = total.saturating_add(u64::from(run.count));
        sorted.push((run.first.0, u64::from(run.first.1), run.end()));
    }
    if total == 0 || total > u64::from(limit) {
        return Err(CoauthorError::MalformedPayload(format!("identity runs name {total} identities; the axis allows 1..={limit}")));
    }
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if let [(participant, _, end), (next_participant, next_start, _)] = pair
            && participant == next_participant
            && next_start < end
        {
            return Err(CoauthorError::MalformedPayload("identity runs repeat an identity".into()));
        }
    }
    u32::try_from(total).map_err(|_| CoauthorError::MalformedPayload("identity runs are too long".into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DeletedAxis {
    Row,
    Column,
}

impl DeletedAxis {
    fn of(rows: bool) -> Self {
        if rows { Self::Row } else { Self::Column }
    }
}

/// The first identity of a deleted run, on one sheet axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeletedKey {
    sheet: LiveId,
    axis: DeletedAxis,
    participant: u32,
    start: u32,
}

/// How long a deleted run is and which participant's ordered entry deleted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeletedOwner {
    len: u32,
    by: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkbookIds {
    pub sheets: OrderedIds,
    pub by_id: BTreeMap<LiveId, SheetIds>,
    /// Who deleted each deleted row and column identity, indexed by run. Every client derives
    /// it from the ordered entries it has applied, never from a payload, so structural undo can
    /// revive only the deleting participant's own identities. It is per-session state: a table
    /// loaded from elsewhere starts empty, which refuses revival rather than allowing it.
    #[serde(skip)]
    deleted_by: BTreeMap<DeletedKey, DeletedOwner>,
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
        Ok(Self { sheets, by_id, deleted_by: BTreeMap::new() })
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

    /// Applies a structural identity change without recording who made it, so the identities
    /// it removes can never be revived by structural undo.
    pub fn apply_structural(&mut self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        let mut next = self.clone();
        next.apply_structural_in_place(change)?;
        *self = next;
        Ok(())
    }

    /// Applies the structural change of an ordered entry and records `participant` as the
    /// deleter of every row or column identity it removes (deleted ones and the tail an insert
    /// pushes off the axis).
    pub(crate) fn apply_structural_owned(&mut self, change: &IdentityStructural, participant: u32) -> Result<(), CoauthorError> {
        let removed = self.removed_runs(change)?;
        let mut next = self.clone();
        next.apply_structural_in_place(change)?;
        if let Some((sheet, axis, runs)) = removed {
            next.record_deleted(sheet, axis, &runs, participant);
        }
        *self = next;
        Ok(())
    }

    /// Takes the live block `remove` out of a row (`rows`) or column axis and revives `restore`
    /// at `insert_at`. Every restored identity must have been deleted by `participant`'s own
    /// ordered entries; the removed block is recorded as deleted by `participant`.
    pub(crate) fn exchange_axis(
        &mut self,
        sheet: LiveId,
        rows: bool,
        remove: &[IdentityRun],
        insert_at: u32,
        restore: &[IdentityRun],
        participant: u32,
    ) -> Result<(), CoauthorError> {
        let axis = DeletedAxis::of(rows);
        let label = if rows { "row" } else { "column" };
        let mut next = self.clone();
        next.claim_deleted(sheet, axis, restore, participant)?;
        let axes = next.live_sheet_mut(sheet)?;
        let ordered = if rows { &mut axes.rows } else { &mut axes.cols };
        remove_block(ordered, remove, label)?;
        revive_runs(ordered, insert_at, restore, label)?;
        next.record_deleted(sheet, axis, remove, participant);
        *self = next;
        Ok(())
    }

    /// Whether every identity in `runs` on this sheet axis was deleted by `participant`. The
    /// answer comes from the ordered entries this client has applied. Each run costs one
    /// ordered-map lookup per deleted run it overlaps, independent of earlier deletions.
    pub(crate) fn deleted_by_participant(&self, sheet: LiveId, rows: bool, runs: &[IdentityRun], participant: u32) -> bool {
        self.owned_pieces(sheet, DeletedAxis::of(rows), runs, participant).is_ok()
    }

    fn owned_pieces(
        &self,
        sheet: LiveId,
        axis: DeletedAxis,
        runs: &[IdentityRun],
        participant: u32,
    ) -> Result<Vec<(DeletedKey, DeletedOwner, u32, u32)>, CoauthorError> {
        let mut pieces = Vec::new();
        for run in runs {
            let end = run.end();
            let mut cursor = u64::from(run.first.1);
            while cursor < end {
                let at = u32::try_from(cursor).map_err(|_| CoauthorError::MalformedPayload("identity run overflows".into()))?;
                let probe = DeletedKey { sheet, axis, participant: run.first.0, start: at };
                let found = self.deleted_by.range(..=probe).next_back().map(|(key, owner)| (*key, *owner));
                let Some((key, owner)) = found.filter(|(key, owner)| {
                    key.sheet == sheet
                        && key.axis == axis
                        && key.participant == run.first.0
                        && cursor < u64::from(key.start).saturating_add(u64::from(owner.len))
                }) else {
                    return Err(CoauthorError::MalformedPayload(format!("{:?} was not deleted by an applied entry", LiveId(run.first.0, at))));
                };
                if owner.by != participant {
                    return Err(CoauthorError::MalformedPayload(format!("{:?} was deleted by another participant", LiveId(run.first.0, at))));
                }
                let piece_end = end.min(u64::from(key.start).saturating_add(u64::from(owner.len)));
                let to =
                    u32::try_from(piece_end - u64::from(key.start)).map_err(|_| CoauthorError::MalformedPayload("identity run overflows".into()))?;
                pieces.push((key, owner, at - key.start, to));
                cursor = piece_end;
            }
        }
        Ok(pieces)
    }

    fn claim_deleted(&mut self, sheet: LiveId, axis: DeletedAxis, runs: &[IdentityRun], participant: u32) -> Result<(), CoauthorError> {
        // One run at a time against the updated index: an earlier run of the same request may
        // already have split an entry that a later run also falls in.
        for run in runs {
            self.claim_run(sheet, axis, *run, participant)?;
        }
        Ok(())
    }

    fn claim_run(&mut self, sheet: LiveId, axis: DeletedAxis, run: IdentityRun, participant: u32) -> Result<(), CoauthorError> {
        for (key, owner, from, to) in self.owned_pieces(sheet, axis, std::slice::from_ref(&run), participant)? {
            self.deleted_by.remove(&key);
            if from > 0 {
                self.deleted_by.insert(key, DeletedOwner { len: from, by: owner.by });
            }
            if to < owner.len {
                let start = key.start.checked_add(to).ok_or_else(|| CoauthorError::MalformedPayload("identity run overflows".into()))?;
                self.deleted_by.insert(DeletedKey { start, ..key }, DeletedOwner { len: owner.len - to, by: owner.by });
            }
        }
        Ok(())
    }

    fn record_deleted(&mut self, sheet: LiveId, axis: DeletedAxis, runs: &[IdentityRun], by: u32) {
        for run in runs {
            self.deleted_by.insert(DeletedKey { sheet, axis, participant: run.first.0, start: run.first.1 }, DeletedOwner { len: run.count, by });
        }
    }

    fn removed_runs(&self, change: &IdentityStructural) -> Result<Option<(LiveId, DeletedAxis, Vec<IdentityRun>)>, CoauthorError> {
        let axes = |sheet: &LiveId| self.by_id.get(sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")));
        Ok(Some(match change {
            IdentityStructural::InsertRows { sheet, count, .. } => (*sheet, DeletedAxis::Row, tail_runs(&axes(sheet)?.rows, *count)?),
            IdentityStructural::InsertColumns { sheet, count, .. } => (*sheet, DeletedAxis::Column, tail_runs(&axes(sheet)?.cols, *count)?),
            IdentityStructural::DeleteRows { sheet, ids, .. } | IdentityStructural::DeleteColumns { sheet, ids, .. } => {
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                let axis = if matches!(change, IdentityStructural::DeleteRows { .. }) { DeletedAxis::Row } else { DeletedAxis::Column };
                (*sheet, axis, compress_runs(sorted))
            }
            IdentityStructural::InsertSheet { .. } | IdentityStructural::DeleteSheet { .. } => return Ok(None),
        }))
    }

    /// Checks a structural identity change without changing this table.
    pub fn validate_structural(&self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        let mut next = self.clone();
        next.apply_structural_in_place(change)
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct OrderedRunWire {
    participant: u32,
    start: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct DeletedRunWire {
    run: OrderedRunWire,
    was_at: u32,
    after: Option<LiveId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderedIdsWire {
    live: Vec<OrderedRunWire>,
    deleted: Vec<DeletedRunWire>,
}

fn ordered_wire(ids: &OrderedIds) -> Result<OrderedIdsWire, CoauthorError> {
    serde_json::from_value(serde_json::to_value(ids).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?)
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

/// The live runs of an axis in position order, each with the position of its first identity.
/// The cost is proportional to the number of runs, not to the identities they hold.
fn positioned_live_runs(ids: &OrderedIds) -> Result<Vec<(u32, IdentityRun)>, CoauthorError> {
    let wire = ordered_wire(ids)?;
    let mut at = 0u32;
    let mut runs = Vec::with_capacity(wire.live.len());
    for run in wire.live {
        runs.push((at, IdentityRun { first: LiveId(run.participant, run.start), count: run.len }));
        at = at.checked_add(run.len).ok_or_else(|| CoauthorError::MalformedPayload("axis length overflows".into()))?;
    }
    Ok(runs)
}

/// The position of the first identity of `run` when the whole run is live and in order on the
/// axis, otherwise `None`. `sorted` indexes `live` by (participant, first counter).
fn locate_run(live: &[(u32, IdentityRun)], sorted: &[usize], run: IdentityRun) -> Option<u32> {
    let index = sorted
        .partition_point(|&index| live.get(index).is_some_and(|(_, candidate)| (candidate.first.0, candidate.first.1) <= (run.first.0, run.first.1)));
    let mut index = *sorted.get(index.checked_sub(1)?)?;
    let (at, holder) = *live.get(index)?;
    if !holder.holds(run.first) {
        return None;
    }
    let start = at.checked_add(run.first.1 - holder.first.1)?;
    let mut next = run.first.1;
    let mut remaining = run.count;
    let mut current = holder;
    loop {
        let available = u32::try_from(current.end().checked_sub(u64::from(next))?).ok()?;
        let taken = available.min(remaining);
        remaining -= taken;
        if remaining == 0 {
            return Some(start);
        }
        next = next.checked_add(taken)?;
        index = index.checked_add(1)?;
        current = live.get(index)?.1;
        if current.first != LiveId(run.first.0, next) {
            return None;
        }
    }
}

/// Where the live block named by `runs` starts, when the runs together occupy one block of
/// consecutive positions (in any run order). `None` when any identity is not live or the block
/// is broken.
pub(crate) fn live_block_position(ids: &OrderedIds, runs: &[IdentityRun]) -> Result<Option<u32>, CoauthorError> {
    let live = positioned_live_runs(ids)?;
    let mut sorted: Vec<usize> = (0..live.len()).collect();
    sorted.sort_unstable_by_key(|&index| live.get(index).map(|(_, run)| (run.first.0, run.first.1)));
    let mut located = Vec::with_capacity(runs.len());
    for run in runs {
        let Some(at) = locate_run(&live, &sorted, *run) else { return Ok(None) };
        located.push((at, run.count));
    }
    located.sort_unstable();
    let Some(&(first, _)) = located.first() else { return Ok(None) };
    let mut expected = first;
    for (at, count) in located {
        if at != expected {
            return Ok(None);
        }
        let Some(next) = expected.checked_add(count) else { return Ok(None) };
        expected = next;
    }
    Ok(Some(first))
}

/// Deletes the live block named by `runs` in one step.
fn remove_block(ids: &mut OrderedIds, runs: &[IdentityRun], label: &str) -> Result<(), CoauthorError> {
    let total = validate_runs(runs, u32::MAX)?;
    let at = live_block_position(ids, runs)?
        .ok_or_else(|| CoauthorError::MalformedPayload(format!("the {label} identities to remove are not one live block")))?;
    ids.delete(at, total)?;
    Ok(())
}

/// Revives the deleted identities `restore`, in order, from position `insert_at`. The work is
/// proportional to the stored runs and the restored runs, not to the identities they hold.
fn revive_runs(ids: &mut OrderedIds, insert_at: u32, restore: &[IdentityRun], label: &str) -> Result<(), CoauthorError> {
    let total = validate_runs(restore, u32::MAX)?;
    let mut wanted: Vec<(u32, u64, u64)> = restore.iter().map(|run| (run.first.0, u64::from(run.first.1), run.end())).collect();
    wanted.sort_unstable();
    let mut wire = ordered_wire(ids)?;
    let mut deleted = Vec::with_capacity(wire.deleted.len().saturating_add(wanted.len()));
    let mut covered = 0u64;
    let piece = |gone: &DeletedRunWire, from: u64, to: u64| -> Result<DeletedRunWire, CoauthorError> {
        let start = u32::try_from(from).map_err(|_| CoauthorError::MalformedPayload("deleted identity run overflows".into()))?;
        let len = u32::try_from(to - from).map_err(|_| CoauthorError::MalformedPayload("deleted identity run overflows".into()))?;
        let offset =
            u32::try_from(from - u64::from(gone.run.start)).map_err(|_| CoauthorError::MalformedPayload("deleted identity run overflows".into()))?;
        Ok(DeletedRunWire {
            run: OrderedRunWire { participant: gone.run.participant, start, len },
            was_at: gone.was_at.saturating_add(offset),
            after: gone.after,
        })
    };
    for gone in &wire.deleted {
        let gone_start = u64::from(gone.run.start);
        let gone_end = gone_start.saturating_add(u64::from(gone.run.len));
        let mut index = wanted.partition_point(|&(participant, _, end)| (participant, end) <= (gone.run.participant, gone_start));
        let mut kept_from = gone_start;
        while let Some(&(participant, start, end)) = wanted.get(index) {
            if participant != gone.run.participant || start >= gone_end {
                break;
            }
            let cut_from = start.max(gone_start);
            let cut_to = end.min(gone_end);
            if cut_from > kept_from {
                deleted.push(piece(gone, kept_from, cut_from)?);
            }
            covered = covered.saturating_add(cut_to - cut_from);
            kept_from = cut_to;
            index += 1;
        }
        if kept_from < gone_end {
            deleted.push(piece(gone, kept_from, gone_end)?);
        }
    }
    if covered != u64::from(total) {
        return Err(CoauthorError::MalformedPayload(format!("a restored {label} identity is not deleted")));
    }
    wire.deleted = deleted;
    let mut revived: OrderedIds =
        serde_json::from_value(serde_json::to_value(wire).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?)
            .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let mut at = insert_at;
    for run in restore {
        revived.insert(at, run.first, run.count)?;
        at = at.checked_add(run.count).ok_or_else(|| CoauthorError::MalformedPayload(format!("{label} restoration position overflows")))?;
    }
    *ids = revived;
    Ok(())
}

/// The runs of the last `count` live identities, in position order.
pub(crate) fn tail_runs(ids: &OrderedIds, count: u32) -> Result<Vec<IdentityRun>, CoauthorError> {
    let live = positioned_live_runs(ids)?;
    let mut remaining = count;
    let mut tail = Vec::new();
    for (_, run) in live.iter().rev() {
        if remaining == 0 {
            break;
        }
        let taken = run.count.min(remaining);
        let skip = run.count - taken;
        let first = run.first.1.checked_add(skip).ok_or_else(|| CoauthorError::MalformedPayload("identity run overflows".into()))?;
        tail.push(IdentityRun { first: LiveId(run.first.0, first), count: taken });
        remaining -= taken;
    }
    if remaining > 0 {
        return Err(CoauthorError::MalformedPayload("structural identity tail is too short".into()));
    }
    tail.reverse();
    Ok(tail)
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
    // Delete each block of consecutive positions in one step, from the bottom up. That leaves
    // the same lookups as deleting one identity at a time, as one deleted run per block.
    positions.sort_unstable();
    let mut blocks: Vec<(u32, u32)> = Vec::new();
    for at in positions {
        match blocks.last_mut() {
            Some((start, count)) if start.checked_add(*count) == Some(at) => *count += 1,
            _ => blocks.push((at, 1)),
        }
    }
    for (at, count) in blocks.into_iter().rev() {
        ids.delete(at, count)?;
    }
    Ok(())
}

impl From<IdError> for CoauthorError {
    fn from(value: IdError) -> Self {
        CoauthorError::Identity(value)
    }
}
