//! Presence: where one person is working, named by lasting identity.
//!
//! A note is an opaque string for the room: it carries the person's sheet, active cell, one
//! selected range and whether they are typing in a cell, and nothing else. It is encoded from the
//! sender's own view and decoded against the reader's current agreed identity tables. Neither
//! side reads or changes the workbook's content.

use collab_live::ids::LiveId;
use gridcraft_core::RangeRef;
use serde::{Deserialize, Serialize};

use super::{CellId, IdentityRange, WorkbookIds};
use crate::Session;

/// The largest presence note, in bytes, sent or read.
pub const MAX_PRESENCE_BYTES: usize = 1024;

/// The shortest time between two notes from one editor: at most ten a second.
pub const MIN_PRESENCE_INTERVAL_MS: u64 = 100;

/// One person's place, by identity.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    pub sheet: LiveId,
    pub active: CellId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<IdentityRange>,
    pub editing: bool,
}

/// Another person's place, resolved to positions in this editor's workbook for drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShownPresence {
    /// The sheet's position.
    pub sheet: usize,
    /// The active cell as zero-based (row, column).
    pub active: (u32, u32),
    /// The selected range when it is more than the active cell.
    pub range: Option<RangeRef>,
    /// Whether the person is typing in the active cell (never what they type).
    pub editing: bool,
}

/// Reads the person's own view and names it by identity. `None` when the view cannot be named,
/// which only happens without a document.
pub(super) fn encode(session: &Session, ids: &WorkbookIds, editing: bool) -> Option<String> {
    let document = session.doc().ok()?;
    let position = document.wb.active_sheet();
    let sheet = ids.sheet_id(position).ok()?;
    let active = ids.cell_id(position, document.selection.active).ok()?;
    let current = document.selection.current();
    let range = if current == RangeRef::cell(document.selection.active) { None } else { ids.range_id(position, current).ok() };
    let note = serde_json::to_string(&Presence { sheet, active, range, editing }).ok()?;
    (note.len() <= MAX_PRESENCE_BYTES).then_some(note)
}

/// Resolves another person's note against `ids`. Malformed, oversized or inconsistent notes, and
/// notes on a sheet, row or column that is gone, are dropped without error.
pub(super) fn decode(ids: &WorkbookIds, note: &str) -> Option<ShownPresence> {
    if note.len() > MAX_PRESENCE_BYTES {
        return None;
    }
    let presence: Presence = serde_json::from_str(note).ok()?;
    let sheet = ids.resolve_sheet(presence.sheet).ok()?;
    if presence.active.sheet != presence.sheet {
        return None;
    }
    let (_, active) = ids.resolve_cell(presence.active).ok()?;
    let range = match presence.range {
        Some(range) if range.sheet == presence.sheet => Some(ids.resolve_range(range).ok()?.1),
        Some(_) => return None,
        None => None,
    };
    Some(ShownPresence { sheet, active: (active.row, active.col), range, editing: presence.editing })
}
