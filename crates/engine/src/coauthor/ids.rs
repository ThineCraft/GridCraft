use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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

/// One sheet's row and column identity tables. [`WorkbookIds`] keeps each behind an `Arc`, so
/// a snapshot shares the tables and a change copies only the sheet it touches.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SheetIds {
    pub id: LiveId,
    pub rows: OrderedIds,
    pub cols: OrderedIds,
}

/// Copying a sheet's tables costs time in proportion to their runs, so every copy is counted
/// (see [`identity_runs_visited`]).
impl Clone for SheetIds {
    fn clone(&self) -> Self {
        count_runs_visited(self.rows.runs().saturating_add(self.cols.runs()));
        Self { id: self.id, rows: self.rows.clone(), cols: self.cols.clone() }
    }
}

thread_local! {
    /// Identity runs this thread has copied or walked one by one (see [`identity_runs_visited`]).
    static RUNS_VISITED: Cell<u64> = const { Cell::new(0) };
}

fn count_runs_visited(runs: usize) {
    let runs = u64::try_from(runs).unwrap_or(u64::MAX);
    RUNS_VISITED.with(|visited| visited.set(visited.get().saturating_add(runs)));
}

/// How many identity runs this thread has copied or walked one by one: each copy of a sheet's
/// tables and each pass over a whole axis adds its runs. Lookups are not counted, because the
/// library answers them by binary search over its index. A change that touches no axis (a
/// `cell.set`, say) adds nothing however fragmented the axes are; the fragmentation tests and
/// timings read this. It counts work only and holds no content.
#[doc(hidden)]
pub fn identity_runs_visited() -> u64 {
    RUNS_VISITED.with(Cell::get)
}

/// [`IdError::TooFragmented`] as the co-authoring error every caller matches on.
pub(crate) fn fragmentation_error(error: IdError) -> CoauthorError {
    match error {
        IdError::TooFragmented { .. } => CoauthorError::TooFragmented,
        other => CoauthorError::Identity(other),
    }
}

/// Whether inserting `count` identities at `at` keeps the axis within the live limits. The
/// axis keeps its fixed length, so the insert also takes `count` identities off its end: two
/// blocks. The library counts each block at its worst, so a change this accepts is never
/// refused part-way through.
///
/// `OrderedIds::check_fragmentation` uses only the number of blocks and the table's run count,
/// never the `(at, count)` positions. The positions here and in the other checks describe the
/// blocks for a reader; making them exact would not change any decision.
pub(crate) fn check_insert_fragmentation(ordered: &OrderedIds, at: u32, count: u32) -> Result<(), CoauthorError> {
    ordered.check_fragmentation(&[(at, count), (ordered.len(), count)]).map_err(fragmentation_error)
}

/// Whether deleting the identities at `positions` keeps the axis within the live limits.
/// Consecutive positions are one block, exactly as [`delete_exact`] deletes them, and the
/// fillers appended at the end of the fixed-length axis are one more.
pub(crate) fn check_delete_fragmentation(ordered: &OrderedIds, mut positions: Vec<u32>) -> Result<(), CoauthorError> {
    positions.sort_unstable();
    positions.dedup();
    let count = u32::try_from(positions.len()).map_err(|_| CoauthorError::MalformedPayload("too many structural identities".into()))?;
    let mut blocks: Vec<(u32, u32)> = Vec::new();
    for at in positions {
        match blocks.last_mut() {
            Some((start, len)) if start.checked_add(*len) == Some(at) => *len += 1,
            _ => blocks.push((at, 1)),
        }
    }
    blocks.push((ordered.len(), count));
    ordered.check_fragmentation(&blocks).map_err(fragmentation_error)
}

/// The positions of those deletion targets that are live. The others are refused when the
/// deletion is made, so they touch no block.
fn live_positions(ordered: &OrderedIds, targets: &[LiveId]) -> Vec<u32> {
    targets
        .iter()
        .filter_map(|id| match ordered.find(*id) {
            Lookup::At(at) => Some(at),
            Lookup::Deleted { .. } | Lookup::Unknown => None,
        })
        .collect()
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

/// One sheet's deletion owners. Copying it costs time in proportion to its entries, so every
/// copy is counted (see [`identity_runs_visited`]).
#[derive(Debug, Default, PartialEq, Eq)]
struct SheetOwners(BTreeMap<DeletedKey, DeletedOwner>);

impl Clone for SheetOwners {
    fn clone(&self) -> Self {
        count_runs_visited(self.0.len());
        Self(self.0.clone())
    }
}

/// Who deleted each deleted row and column identity, per sheet. Like the sheets' identity
/// tables, each sheet's owners sit behind an `Arc`: a snapshot shares them and a change copies
/// only the sheet it touches. A sheet with no owners has no entry, so equal tables compare
/// equal. Iteration is in [`DeletedKey`] order, which sorts by sheet first, exactly as one map
/// over every sheet did, so the session data is unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DeletedOwners(BTreeMap<LiveId, Arc<SheetOwners>>);

impl DeletedOwners {
    fn insert(&mut self, key: DeletedKey, owner: DeletedOwner) -> Option<DeletedOwner> {
        Arc::make_mut(self.0.entry(key.sheet).or_default()).0.insert(key, owner)
    }

    fn remove(&mut self, key: &DeletedKey) {
        if let Some(owners) = self.0.get_mut(&key.sheet) {
            let owners = Arc::make_mut(owners);
            owners.0.remove(key);
            if owners.0.is_empty() {
                self.0.remove(&key.sheet);
            }
        }
    }

    /// The owner entry with the greatest key at or before `probe`, on `probe`'s sheet.
    fn at_or_before(&self, probe: DeletedKey) -> Option<(DeletedKey, DeletedOwner)> {
        self.0.get(&probe.sheet)?.0.range(..=probe).next_back().map(|(key, owner)| (*key, *owner))
    }

    fn iter(&self) -> impl Iterator<Item = (&DeletedKey, &DeletedOwner)> {
        self.0.values().flat_map(|owners| owners.0.iter())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkbookIds {
    pub sheets: OrderedIds,
    pub by_id: BTreeMap<LiveId, Arc<SheetIds>>,
    /// Stable XLSX `sheetId` values, including identities of deleted sheets. Imported nonzero
    /// ids are preserved; sheets created in a live session receive a deterministic unused id.
    file_ids: BTreeMap<LiveId, u32>,
    /// Imported chart sheets share the XLSX `sheetId` namespace with worksheets even though
    /// GridCraft keeps them as opaque content.
    reserved_file_ids: BTreeSet<u32>,
    /// Who deleted each deleted row and column identity, indexed by run. It is derived from
    /// ordered entries while the room is live and stored in validated checkpoint session data,
    /// so a later joiner makes exactly the same structural-undo decision as its peers.
    deleted_by: DeletedOwners,
}

/// The most identity session data one checkpoint may carry.
pub const MAX_SESSION_DATA_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SheetMapWire {
    key: LiveId,
    value: SheetIdsWire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SheetIdsWire {
    id: LiveId,
    rows: OrderedIdsWire,
    cols: OrderedIdsWire,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeletedOwnerWire {
    sheet: LiveId,
    rows: bool,
    first: LiveId,
    count: u32,
    by: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIdWire {
    sheet: LiveId,
    id: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkbookIdsWire {
    sheets: OrderedIdsWire,
    by_id: Vec<SheetMapWire>,
    file_ids: Vec<FileIdWire>,
    reserved_file_ids: Vec<u32>,
    deleted_by: Vec<DeletedOwnerWire>,
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
    #[error("the live identity axis would become too fragmented")]
    TooFragmented,
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
            by_id.insert(id, Arc::new(SheetIds { id, rows: OrderedIds::base(MAX_ROWS), cols: OrderedIds::base(MAX_COLS) }));
        }
        let (file_ids, reserved_file_ids) = canonical_file_ids(wb, &sheets)?;
        Ok(Self { sheets, by_id, file_ids, reserved_file_ids, deleted_by: DeletedOwners::default() })
    }

    /// Loads and validates the opaque identity data stored beside a checkpoint.
    pub fn from_session_data(bytes: &[u8], wb: &Workbook) -> Result<Self, CoauthorError> {
        if bytes.len() > MAX_SESSION_DATA_BYTES {
            return Err(CoauthorError::OversizedSessionData { bytes: bytes.len(), max: MAX_SESSION_DATA_BYTES });
        }
        let wire: WorkbookIdsWire = serde_json::from_slice(bytes).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        let sheets = ordered_from_wire(wire.sheets)?;

        let mut by_id = BTreeMap::new();
        for entry in wire.by_id {
            if entry.key != entry.value.id {
                return Err(CoauthorError::MalformedPayload("a sheet identity does not match its map key".into()));
            }
            let value =
                Arc::new(SheetIds { id: entry.value.id, rows: ordered_from_wire(entry.value.rows)?, cols: ordered_from_wire(entry.value.cols)? });
            if by_id.insert(entry.key, value).is_some() {
                return Err(CoauthorError::MalformedPayload("the sheet identity map contains a duplicate key".into()));
            }
        }

        let mut file_ids = BTreeMap::new();
        for entry in wire.file_ids {
            if entry.id == 0 {
                return Err(CoauthorError::MalformedPayload("a worksheet file identity is zero".into()));
            }
            if file_ids.insert(entry.sheet, entry.id).is_some() {
                return Err(CoauthorError::MalformedPayload("the worksheet file identity map contains a duplicate sheet".into()));
            }
        }
        let reserved_count = wire.reserved_file_ids.len();
        let reserved_file_ids: BTreeSet<u32> = wire.reserved_file_ids.into_iter().collect();
        if reserved_file_ids.len() != reserved_count {
            return Err(CoauthorError::MalformedPayload("preserved file identities are repeated".into()));
        }

        let mut deleted_by = DeletedOwners::default();
        for owner in wire.deleted_by {
            if owner.count == 0 || u64::from(owner.first.1).saturating_add(u64::from(owner.count)) > u64::from(u32::MAX) + 1 {
                return Err(CoauthorError::MalformedPayload("a deletion-owner run is empty or overflows".into()));
            }
            if owner.by == 0 {
                return Err(CoauthorError::MalformedPayload("participant zero cannot own a deletion".into()));
            }
            let axis = DeletedAxis::of(owner.rows);
            let key = DeletedKey { sheet: owner.sheet, axis, participant: owner.first.0, start: owner.first.1 };
            if deleted_by.insert(key, DeletedOwner { len: owner.count, by: owner.by }).is_some() {
                return Err(CoauthorError::MalformedPayload("the deletion-owner table contains a duplicate run".into()));
            }
        }

        let ids = Self { sheets, by_id, file_ids, reserved_file_ids, deleted_by };
        ids.validate_graph(wb)?;
        ids.validate_deleted_owners()?;
        Ok(ids)
    }

    /// Forgets every deleted row and column identity, and who deleted it, on every sheet;
    /// returns how many deleted runs were dropped. Sheet identities are kept. Every editor of a
    /// session that starts from an earlier session's checkpoint does this as it loads it, so
    /// deleted runs no longer pile up from session to session towards
    /// [`OrderedIds::MAX_RUNS`]. The result depends only on the tables, so all editors agree.
    pub fn compact_deleted_lines(&mut self) -> usize {
        let mut dropped = 0usize;
        for sheet in self.by_id.values_mut() {
            if sheet.rows.total_runs() == sheet.rows.runs() && sheet.cols.total_runs() == sheet.cols.runs() {
                continue;
            }
            let sheet = Arc::make_mut(sheet);
            dropped = dropped.saturating_add(sheet.rows.compact()).saturating_add(sheet.cols.compact());
        }
        // Deletion owners name only deleted row and column runs, all of which are gone now.
        self.deleted_by = DeletedOwners::default();
        dropped
    }

    /// Serialises the validated identity graph stored beside a checkpoint.
    pub fn session_data(&self) -> Result<Vec<u8>, CoauthorError> {
        self.validate_without_workbook()?;
        self.validate_deleted_owners()?;
        let by_id = self
            .by_id
            .iter()
            .map(|(&key, value)| {
                Ok(SheetMapWire { key, value: SheetIdsWire { id: value.id, rows: ordered_wire(&value.rows)?, cols: ordered_wire(&value.cols)? } })
            })
            .collect::<Result<Vec<_>, CoauthorError>>()?;
        let wire = WorkbookIdsWire {
            sheets: ordered_wire(&self.sheets)?,
            by_id,
            file_ids: self.file_ids.iter().map(|(&sheet, &id)| FileIdWire { sheet, id }).collect(),
            reserved_file_ids: self.reserved_file_ids.iter().copied().collect(),
            deleted_by: self
                .deleted_by
                .iter()
                .map(|(key, owner)| DeletedOwnerWire {
                    sheet: key.sheet,
                    rows: key.axis == DeletedAxis::Row,
                    first: LiveId(key.participant, key.start),
                    count: owner.len,
                    by: owner.by,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&wire).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        if bytes.len() > MAX_SESSION_DATA_BYTES {
            return Err(CoauthorError::OversizedSessionData { bytes: bytes.len(), max: MAX_SESSION_DATA_BYTES });
        }
        Ok(bytes)
    }

    fn validate_without_workbook(&self) -> Result<(), CoauthorError> {
        self.sheets.validate()?;
        if self.file_ids.len() != self.by_id.len() || self.file_ids.keys().ne(self.by_id.keys()) {
            return Err(CoauthorError::MalformedPayload("worksheet file identities do not correspond one-for-one with sheet identities".into()));
        }
        let mut unique_file_ids = BTreeSet::new();
        for &file_id in self.file_ids.values() {
            if file_id == 0 || !unique_file_ids.insert(file_id) {
                return Err(CoauthorError::MalformedPayload("worksheet file identities are zero or repeated".into()));
            }
        }
        if self.reserved_file_ids.contains(&0) || self.reserved_file_ids.iter().any(|file_id| unique_file_ids.contains(file_id)) {
            return Err(CoauthorError::MalformedPayload("preserved and worksheet file identities overlap or contain zero".into()));
        }
        for (&key, value) in &self.by_id {
            if key != value.id {
                return Err(CoauthorError::MalformedPayload("a sheet identity does not match its map key".into()));
            }
            if matches!(self.sheets.find(key), Lookup::Unknown) {
                return Err(CoauthorError::MalformedPayload("the sheet identity map contains an unknown identity".into()));
            }
            value.rows.validate()?;
            value.cols.validate()?;
            if value.rows.len() != MAX_ROWS || value.cols.len() != MAX_COLS {
                return Err(CoauthorError::MalformedPayload("a worksheet identity axis has the wrong length".into()));
            }
        }
        for position in 0..self.sheets.len() {
            let id = self.sheets.id_at(position).ok_or_else(|| CoauthorError::MalformedPayload("a live sheet identity is missing".into()))?;
            if !self.by_id.contains_key(&id) {
                return Err(CoauthorError::MalformedPayload("the sheet identity map is missing a live sheet".into()));
            }
        }
        Ok(())
    }

    pub(super) fn restore_file_ids(&self, wb: &mut Workbook) -> Result<(), CoauthorError> {
        for position in 0..wb.sheets.len() {
            let sheet = self.sheet_id(position)?;
            let file_id = self
                .file_ids
                .get(&sheet)
                .copied()
                .ok_or_else(|| CoauthorError::MalformedPayload("a live sheet has no worksheet file identity".into()))?;
            let model_sheet =
                wb.sheets.get_mut(position).ok_or_else(|| CoauthorError::MalformedPayload("a live sheet has no workbook sheet".into()))?;
            Arc::make_mut(model_sheet).file_id = file_id;
        }
        Ok(())
    }

    fn validate_graph(&self, wb: &Workbook) -> Result<(), CoauthorError> {
        self.validate_without_workbook()?;
        let sheet_count = u32::try_from(wb.sheets.len())
            .map_err(|_| CoauthorError::MalformedPayload("the workbook has too many sheets for live identities".into()))?;
        if self.sheets.len() != sheet_count {
            return Err(CoauthorError::MalformedPayload("live sheet identities do not correspond one-for-one with the workbook".into()));
        }
        let workbook_reserved: BTreeSet<u32> =
            wb.preserved.iter().flat_map(|kept| kept.sheets.iter().map(|sheet| sheet.sheet_id)).filter(|&id| id != 0).collect();
        if workbook_reserved != self.reserved_file_ids {
            return Err(CoauthorError::MalformedPayload("preserved file identities do not match the checkpoint workbook".into()));
        }
        for (position, sheet) in wb.sheets.iter().enumerate() {
            let identity = self.sheet_id(position)?;
            let mapped = self
                .file_ids
                .get(&identity)
                .copied()
                .ok_or_else(|| CoauthorError::MalformedPayload("a live sheet has no worksheet file identity".into()))?;
            if sheet.file_id != 0 && sheet.file_id != mapped {
                return Err(CoauthorError::MalformedPayload("a worksheet file identity does not match the checkpoint workbook".into()));
            }
        }
        Ok(())
    }

    fn validate_deleted_owners(&self) -> Result<(), CoauthorError> {
        let coverage = deleted_coverage(&self.by_id)?;
        let mut previous: Option<(DeletedKey, u64)> = None;
        for (&key, owner) in self.deleted_by.iter() {
            if owner.len == 0 || owner.by == 0 {
                return Err(CoauthorError::MalformedPayload("a deletion-owner run is empty or has no participant".into()));
            }
            let end = u64::from(key.start).saturating_add(u64::from(owner.len));
            if end > u64::from(u32::MAX) + 1 {
                return Err(CoauthorError::MalformedPayload("a deletion-owner run overflows".into()));
            }
            if let Some((prior, prior_end)) = previous
                && prior.sheet == key.sheet
                && prior.axis == key.axis
                && prior.participant == key.participant
                && u64::from(key.start) < prior_end
            {
                return Err(CoauthorError::MalformedPayload("deletion-owner runs overlap".into()));
            }
            if !deleted_run_is_covered(&coverage, key, owner.len) {
                return Err(CoauthorError::MalformedPayload("a deletion owner names an identity that is not deleted".into()));
            }
            previous = Some((key, end));
        }
        Ok(())
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
        self.check_exchange_fragmentation(sheet, rows, remove, insert_at, restore)?;
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
                let found = self.deleted_by.at_or_before(probe);
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

    /// The check [`Self::exchange_axis`] makes before it changes anything: the live block it
    /// removes, and each run it revives (a revived run can also cut a deleted run in two).
    pub(crate) fn check_exchange_fragmentation(
        &self,
        sheet: LiveId,
        rows: bool,
        remove: &[IdentityRun],
        insert_at: u32,
        restore: &[IdentityRun],
    ) -> Result<(), CoauthorError> {
        let axes = self.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
        let ordered = if rows { &axes.rows } else { &axes.cols };
        // The library decides from the number of blocks alone (see
        // `check_insert_fragmentation`); the positions are only descriptive, so the removed
        // block is not looked up.
        let removed = remove.iter().fold(0u32, |total, run| total.saturating_add(run.count));
        let mut blocks = Vec::with_capacity(restore.len().saturating_add(1));
        blocks.push((insert_at, removed));
        let mut at = insert_at;
        for run in restore {
            blocks.push((at, run.count));
            at = at.saturating_add(run.count);
        }
        ordered.check_fragmentation(&blocks).map_err(fragmentation_error)
    }

    /// Refuses a structural change that would pass the live limits on its axis, before
    /// anything is copied or changed.
    fn check_structural_fragmentation(&self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        let axis = |sheet: &LiveId, rows: bool| {
            self.by_id
                .get(sheet)
                .map(|axes| if rows { &axes.rows } else { &axes.cols })
                .ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))
        };
        match change {
            IdentityStructural::InsertRows { sheet, before, count, .. } | IdentityStructural::InsertColumns { sheet, before, count, .. } => {
                let ordered = axis(sheet, matches!(change, IdentityStructural::InsertRows { .. }))?;
                check_insert_fragmentation(ordered, position(ordered, *before, "axis")?, *count)
            }
            IdentityStructural::DeleteRows { sheet, ids, .. } | IdentityStructural::DeleteColumns { sheet, ids, .. } => {
                let ordered = axis(sheet, matches!(change, IdentityStructural::DeleteRows { .. }))?;
                check_delete_fragmentation(ordered, live_positions(ordered, ids))
            }
            IdentityStructural::InsertSheet { .. } | IdentityStructural::DeleteSheet { .. } => Ok(()),
        }
    }

    fn apply_structural_in_place(&mut self, change: &IdentityStructural) -> Result<(), CoauthorError> {
        // Checked first, so a refused change has copied nothing and says why on every machine.
        self.check_structural_fragmentation(change)?;
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
                let file_id = next_file_id(self.file_ids.values().copied().chain(self.reserved_file_ids.iter().copied()))?;
                let at = position(&self.sheets, *before, "sheet")?;
                self.sheets.insert(at, *id, 1)?;
                self.by_id.insert(*id, Arc::new(SheetIds { id: *id, rows: OrderedIds::base(MAX_ROWS), cols: OrderedIds::base(MAX_COLS) }));
                self.file_ids.insert(*id, file_id);
            }
            IdentityStructural::DeleteSheet { id } => delete_exact(&mut self.sheets, &[*id], "sheet")?,
        }
        Ok(())
    }

    fn live_sheet_mut(&mut self, id: LiveId) -> Result<&mut SheetIds, CoauthorError> {
        match self.sheets.find(id) {
            Lookup::At(_) => Ok(Arc::make_mut(self.by_id.get_mut(&id).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {id:?}")))?)),
            Lookup::Deleted { .. } => Err(CoauthorError::DeletedTarget(format!("sheet {id:?}"))),
            Lookup::Unknown => Err(CoauthorError::UnknownIdentity(format!("sheet {id:?}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderedRunWire {
    participant: u32,
    start: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeletedRunWire {
    run: OrderedRunWire,
    was_at: u32,
    after: Option<LiveId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderedIdsWire {
    live: Vec<OrderedRunWire>,
    deleted: Vec<DeletedRunWire>,
}

fn canonical_file_ids(wb: &Workbook, sheets: &OrderedIds) -> Result<(BTreeMap<LiveId, u32>, BTreeSet<u32>), CoauthorError> {
    let reserved: BTreeSet<u32> = wb.preserved.iter().flat_map(|kept| kept.sheets.iter().map(|sheet| sheet.sheet_id)).filter(|&id| id != 0).collect();
    let mut used: BTreeSet<u32> = wb.sheets.iter().map(|sheet| sheet.file_id).filter(|&id| id != 0).chain(reserved.iter().copied()).collect();
    let mut claimed_imported = BTreeSet::new();
    let mut file_ids = BTreeMap::new();
    for (position, sheet) in wb.sheets.iter().enumerate() {
        let position = u32::try_from(position).map_err(|_| CoauthorError::MalformedPayload("too many sheets for file identities".into()))?;
        let identity = sheets.id_at(position).ok_or_else(|| CoauthorError::MalformedPayload("a worksheet identity is missing".into()))?;
        let file_id = if sheet.file_id != 0 && !reserved.contains(&sheet.file_id) && claimed_imported.insert(sheet.file_id) {
            sheet.file_id
        } else {
            take_next_file_id(&mut used)?
        };
        file_ids.insert(identity, file_id);
    }
    Ok((file_ids, reserved))
}

fn next_file_id(ids: impl IntoIterator<Item = u32>) -> Result<u32, CoauthorError> {
    let mut used: BTreeSet<u32> = ids.into_iter().filter(|&id| id != 0).collect();
    take_next_file_id(&mut used)
}

fn take_next_file_id(used: &mut BTreeSet<u32>) -> Result<u32, CoauthorError> {
    let mut candidate = 1u32;
    while used.contains(&candidate) {
        candidate = candidate.checked_add(1).ok_or_else(|| CoauthorError::MalformedPayload("worksheet file identities are exhausted".into()))?;
    }
    used.insert(candidate);
    Ok(candidate)
}

fn ordered_wire(ids: &OrderedIds) -> Result<OrderedIdsWire, CoauthorError> {
    let wire: OrderedIdsWire = serde_json::from_value(serde_json::to_value(ids).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?)
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    count_runs_visited(wire.live.len().saturating_add(wire.deleted.len()));
    Ok(wire)
}

fn ordered_from_wire(wire: OrderedIdsWire) -> Result<OrderedIds, CoauthorError> {
    serde_json::from_value(serde_json::to_value(wire).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?)
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

type DeletedCoverage = BTreeMap<(LiveId, DeletedAxis, u32), Vec<(u64, u64)>>;

/// Indexes and merges every deleted identity run once. Checkpoint metadata may contain many
/// owner runs, so rebuilding and sorting an entire million-cell axis per owner would let a
/// fragmented payload turn validation quadratic.
fn deleted_coverage(by_id: &BTreeMap<LiveId, Arc<SheetIds>>) -> Result<DeletedCoverage, CoauthorError> {
    let mut coverage: DeletedCoverage = BTreeMap::new();
    for (&sheet, axes) in by_id {
        for (axis, ids) in [(DeletedAxis::Row, &axes.rows), (DeletedAxis::Column, &axes.cols)] {
            for gone in ordered_wire(ids)?.deleted {
                let start = u64::from(gone.run.start);
                let end = start.saturating_add(u64::from(gone.run.len));
                coverage.entry((sheet, axis, gone.run.participant)).or_default().push((start, end));
            }
        }
    }
    for intervals in coverage.values_mut() {
        intervals.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(intervals.len());
        for (start, end) in intervals.drain(..) {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        *intervals = merged;
    }
    Ok(coverage)
}

fn deleted_run_is_covered(coverage: &DeletedCoverage, key: DeletedKey, count: u32) -> bool {
    let Some(intervals) = coverage.get(&(key.sheet, key.axis, key.participant)) else { return false };
    let start = u64::from(key.start);
    let end = start.saturating_add(u64::from(count));
    let at = intervals.partition_point(|(_, interval_end)| *interval_end <= start);
    intervals.get(at).is_some_and(|(interval_start, interval_end)| *interval_start <= start && end <= *interval_end)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A deletion is its blocks of consecutive rows plus the fillers appended at the end of the
    /// axis, so 1,023 separate rows are the most one deletion may name; a refused deletion
    /// changes nothing.
    #[test]
    fn deletion_blocks_count_the_appended_fillers() {
        let workbook = Workbook::new();
        let mut ids = WorkbookIds::from_workbook(&workbook).expect("base identities build");
        let sheet = ids.sheet_id(0).expect("base sheet exists");
        let alternate = |ids: &WorkbookIds, count: u32| -> Vec<LiveId> { (0..count).filter_map(|at| ids.by_id[&sheet].rows.id_at(at * 2)).collect() };
        let before = ids.clone();
        let over = alternate(&ids, 1_024);
        let result = ids.apply_structural(&IdentityStructural::DeleteRows { sheet, ids: over, first: LiveId(1, 0) });
        assert!(matches!(result, Err(CoauthorError::TooFragmented)), "{result:?}");
        assert_eq!(ids, before, "a refused deletion changes nothing");
        let most = alternate(&ids, 1_023);
        ids.apply_structural(&IdentityStructural::DeleteRows { sheet, ids: most, first: LiveId(1, 0) }).expect("1,023 blocks and the fillers fit");
        // Consecutive rows are one block whatever their number.
        let block: Vec<LiveId> = (5_000..7_000u32).filter_map(|at| ids.by_id[&sheet].rows.id_at(at)).collect();
        ids.apply_structural(&IdentityStructural::DeleteRows { sheet, ids: block, first: LiveId(1, 1_023) }).expect("one block of 2,000 rows");
    }

    #[test]
    fn fragmented_multi_owner_checkpoint_metadata_round_trips() {
        let workbook = Workbook::new();
        let mut ids = WorkbookIds::from_workbook(&workbook).expect("base identities build");
        let sheet = ids.sheet_id(0).expect("base sheet has an identity");
        let mut next = [0u32; 2];
        for position in 0..256u32 {
            let by = 1 + position % 2;
            let slot = usize::try_from(by - 1).expect("test participant index fits");
            let first = LiveId(by, next[slot]);
            next[slot] += 1;
            let row = ids.by_id.get(&sheet).and_then(|axes| axes.rows.id_at(position)).expect("fragmented source row stays live");
            ids.apply_structural_owned(&IdentityStructural::DeleteRows { sheet, ids: vec![row], first }, by).expect("fragmented deletion applies");
        }

        let bytes = ids.session_data().expect("fragmented ownership serializes");
        let loaded = WorkbookIds::from_session_data(&bytes, &workbook).expect("fragmented ownership validates once per axis");
        assert_eq!(loaded, ids);
    }

    #[test]
    fn live_sheet_ids_reserve_preserved_chart_sheet_ids() {
        let mut workbook = Workbook::new();
        Arc::make_mut(workbook.sheets.get_mut(0).expect("worksheet exists")).file_id = 2;
        workbook.preserved = Some(Arc::new(gridcraft_model::Preserved {
            sheets: vec![
                gridcraft_model::PreservedSheet { name: "Before".into(), before: 0, sheet_id: 1, ..Default::default() },
                gridcraft_model::PreservedSheet { name: "After".into(), before: 1, sheet_id: 4, ..Default::default() },
            ],
            ..Default::default()
        }));
        let mut ids = WorkbookIds::from_workbook(&workbook).expect("mixed sheet identities build");
        let inserted = LiveId(7, 0);
        ids.apply_structural_owned(&IdentityStructural::InsertSheet { before: IdentityPosition::End, id: inserted }, 7)
            .expect("live worksheet inserts around preserved chart sheets");

        assert_eq!(ids.file_ids.get(&inserted), Some(&3), "the inserted worksheet does not steal either chart sheet id");
        assert_eq!(ids.reserved_file_ids, BTreeSet::from([1, 4]));
    }
}
