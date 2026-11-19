//! The engine's surface for a live host: which commands are shared, which stay personal and
//! which are refused, explicit targets on the sender's side, the base a live editor starts from,
//! and the notices the host shows.

use std::fmt;

use collab_core::ids::Seq;
use collab_live::client::Stamp;
use gridcraft_core::{RangeRef, col_to_letters};
use gridcraft_model::{HAlign, Underline};
use protocol::live::{ParticipantNo, RefusedReason};
use serde::Serialize;
use serde_json::{Map, Value as Json};

use super::{CoauthorError, is_shared_command};
use crate::Session;

/// What a refused command says while a document is edited together.
pub const UNSHARED_REASON: &str = "Not available while editing together";

/// How a live editor treats one command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A shared change: made explicit, then sent through [`super::GridCoauthor::submit`].
    Shared,
    /// Personal view work, run on this editor only and never sent.
    Local,
    /// Not shared yet; it does nothing and the person is told why.
    Refused(&'static str),
}

/// Commands that change only the person's own view (C11). Checked read-only queries are
/// personal too; see [`LOCAL_QUERIES`].
pub const LOCAL_COMMANDS: &[&str] = &[
    "selection.set",
    "selection.move",
    "selection.next",
    "selection.row",
    "selection.column",
    "selection.currentRegion",
    "edit.selectAll",
    "edit.goTo",
    "edit.goToSpecial",
    "sheet.activate",
    "sheet.next",
    "sheet.previous",
    "view.zoom",
    "view.zoom100",
    "view.zoomToSelection",
    "view.freezePanes",
    "view.unfreezePanes",
    "view.freezeTopRow",
    "view.freezeFirstColumn",
    "view.gridlines",
    "edit.copy",
    "edit.find",
];

/// Read-only queries a live document runs as personal. Each was checked to read the workbook
/// without writing it; `local_commands_leave_the_shared_workbook_unchanged` runs them all and
/// compares the entered fingerprint. A query is not personal just because it is not journaled:
/// `formulas.watchWindow` keeps its list in a hidden defined name and `macro.check` dry-runs
/// commands on the workbook, so both stay refused.
pub const LOCAL_QUERIES: &[&str] = &[
    "cell.get",
    "sheet.read",
    "selection.stats",
    "document.inspect",
    "window.list",
    "app.commands",
    "history.list",
    "formulas.evaluate",
    "formulas.evaluateFormula",
    "formulas.tracePrecedents",
    "formulas.traceDependents",
    "formulas.nameManager",
    "formulas.functions",
    "formulas.recentlyUsed",
    "review.spelling",
    "review.thesaurus",
    "review.workbookStatistics",
    "review.showComments",
    "review.showAllNotes",
    "view.formulaBar",
    "pivot.fields",
    "pivot.fieldList",
    "pivot.list",
    "macro.list",
    "macro.get",
    "control.list",
];

/// Whether `id` is personal: a view command in [`LOCAL_COMMANDS`] or a checked read-only query
/// in [`LOCAL_QUERIES`]. Everything else that is not shared is refused.
pub fn is_local_command(id: &str) -> bool {
    !is_shared_command(id) && (LOCAL_COMMANDS.contains(&id) || LOCAL_QUERIES.contains(&id))
}

/// Routes one command in a live document. Shared is exactly [`is_shared_command`].
pub fn route(id: &str) -> Route {
    if is_shared_command(id) {
        Route::Shared
    } else if is_local_command(id) {
        Route::Local
    } else {
        Route::Refused(UNSHARED_REASON)
    }
}

/// Fills the sheet, range, rows or columns and the toggles a shared command would otherwise take
/// from the sender's selection and active sheet, so the entry names its targets explicitly.
/// Runs once, on the sender's side; received entries never pass through it.
pub fn explicit_params(session: &Session, id: &str, params: Json) -> Result<Json, CoauthorError> {
    if !is_shared_command(id) {
        return Err(CoauthorError::UnsupportedCommand(id.to_string()));
    }
    let mut map = match params {
        Json::Object(map) => map,
        Json::Null => Map::new(),
        _ => return Err(CoauthorError::MalformedPayload(format!("`{id}` parameters must be an object"))),
    };
    let document = session.doc()?;
    if id == "home.insertSheet" {
        if !map.contains_key("name") {
            map.insert("name".into(), Json::String(document.wb.next_sheet_name()));
        }
        if !map.contains_key("before") {
            let before = document.wb.active_sheet().saturating_add(1).min(document.wb.sheets.len());
            map.insert("before".into(), Json::from(before));
        }
        return Ok(Json::Object(map));
    }
    let probe = Json::Object(map.clone());
    let sheet = crate::cmd::target_sheet(session, &probe)?;
    map.insert("sheet".into(), Json::from(sheet));
    let selection = &document.selection;
    let has = |map: &Map<String, Json>, key: &str| map.get(key).is_some_and(|value| !value.is_null());
    match id {
        "cell.set" => {
            if !has(&map, "cell") && !has(&map, "range") {
                map.insert("cell".into(), Json::String(selection.active.a1()));
            }
            let typed = map.remove("value");
            let input = map.remove("input").or(typed).filter(|value| !value.is_null());
            if let Some(input) = input {
                map.insert("input".into(), Json::String(crate::cmd::edit::json_to_input(&input)));
            }
        }
        "sheet.rename" | "home.deleteSheet" => {}
        "home.insertRows" | "home.insertColumns" => {
            let rows = id == "home.insertRows";
            let key = if rows { "rows" } else { "cols" };
            let lines = line_source(&map, key).unwrap_or_else(|| selection.current());
            map.remove("range");
            map.insert(key.into(), Json::String(line_text(rows, lines)));
        }
        "home.deleteRows" | "home.deleteColumns" => {
            let rows = id == "home.deleteRows";
            let key = if rows { "rows" } else { "cols" };
            if !has(&map, key) {
                let text = if has(&map, "count") {
                    let lines = line_source(&map, key).unwrap_or_else(|| selection.current());
                    let (start, _) = bounds(rows, lines);
                    line_text(rows, line_range(rows, start, start))
                } else {
                    let ranges: Vec<RangeRef> = match map.get("range").and_then(Json::as_str) {
                        Some(text) => text.split(',').filter_map(crate::cmd::parse_lines).collect(),
                        None => selection.ranges.clone(),
                    };
                    merged_lines(rows, &ranges)
                };
                map.insert(key.into(), Json::String(text));
            }
            map.remove("range");
        }
        // These name the sheet's existing filter: its range, never the selection, which the
        // command would take as the name of a different filter.
        "data.filterBy" | "data.clearFilter" | "data.reapply" | "data.filterRemove" => {
            if !has(&map, "range") {
                let filter = document.wb.sheet(sheet).and_then(|target| target.autofilter.as_ref());
                let Some(filter) = filter else {
                    return Err(CoauthorError::MissingTarget { command: id.to_string(), target: "range".into() });
                };
                map.insert("range".into(), Json::String(filter.range.a1()));
            }
        }
        _ => {
            if !has(&map, "range") && !has(&map, "cell") {
                let ranges: Vec<String> = selection.ranges.iter().map(RangeRef::a1).collect();
                map.insert("range".into(), Json::String(ranges.join(",")));
            }
            style_defaults(session, id, &mut map)?;
        }
    }
    Ok(Json::Object(map))
}

/// Fills the toggles and sizes a style command reads from the active cell when they are absent.
fn style_defaults(session: &Session, id: &str, map: &mut Map<String, Json>) -> Result<(), CoauthorError> {
    let missing = |map: &Map<String, Json>, key: &str| map.get(key).is_none_or(Json::is_null);
    match id {
        "home.bold" | "home.italic" | "home.wrapText" if missing(map, "on") => {
            let style = crate::cmd::format::active_style(session)?;
            let current = match id {
                "home.bold" => style.font.bold,
                "home.italic" => style.font.italic,
                _ => style.align.wrap,
            };
            map.insert("on".into(), Json::Bool(!current));
        }
        // Clicking the active alignment again returns the cell to General: the sender decides,
        // and `on: false` carries it.
        "home.alignLeft" | "home.alignCenter" | "home.alignRight" if missing(map, "on") => {
            let wanted = match id {
                "home.alignLeft" => HAlign::Left,
                "home.alignCenter" => HAlign::Center,
                _ => HAlign::Right,
            };
            let current = crate::cmd::format::active_style(session)?.align.h;
            map.insert("on".into(), Json::Bool(current != wanted));
        }
        "home.underline" | "home.doubleUnderline" => {
            let default = if id == "home.doubleUnderline" { "double" } else { "single" };
            let name = map.get("style").and_then(Json::as_str).unwrap_or(default).to_string();
            let wanted = match name.as_str() {
                "double" => Underline::Double,
                "singleAccounting" => Underline::SingleAccounting,
                "doubleAccounting" => Underline::DoubleAccounting,
                "none" => Underline::None,
                _ => Underline::Single,
            };
            if missing(map, "on") {
                let current = crate::cmd::format::active_style(session)?.font.underline;
                map.insert("on".into(), Json::Bool(current != wanted));
            }
            map.insert("style".into(), Json::String(name));
        }
        "home.increaseFontSize" | "home.decreaseFontSize" if missing(map, "size") => {
            let current = crate::cmd::format::active_style(session)?.font.size;
            let size = crate::cmd::format::stepped_font_size(current, id == "home.increaseFontSize");
            map.insert("size".into(), Json::from(f64::from(size)));
        }
        _ => {}
    }
    Ok(())
}

fn line_source(map: &Map<String, Json>, key: &str) -> Option<RangeRef> {
    map.get(key).and_then(Json::as_str).or_else(|| map.get("range").and_then(Json::as_str)).and_then(crate::cmd::parse_lines)
}

fn bounds(rows: bool, range: RangeRef) -> (u32, u32) {
    let (a, b) = if rows { (range.start.row, range.end.row) } else { (range.start.col, range.end.col) };
    (a.min(b), a.max(b))
}

fn line_range(rows: bool, start: u32, end: u32) -> RangeRef {
    if rows { RangeRef::rows(start, end) } else { RangeRef::cols(start, end) }
}

fn line_text(rows: bool, range: RangeRef) -> String {
    let (start, end) = bounds(rows, range);
    if rows { format!("{}:{}", u64::from(start) + 1, u64::from(end) + 1) } else { format!("{}:{}", col_to_letters(start), col_to_letters(end)) }
}

/// The lines under `ranges`, sorted and merged where they touch, as `2:3,5:5` or `B:C,E:E`.
fn merged_lines(rows: bool, ranges: &[RangeRef]) -> String {
    let mut spans: Vec<(u32, u32)> = ranges.iter().map(|range| bounds(rows, *range)).collect();
    spans.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged.into_iter().map(|(start, end)| line_text(rows, line_range(rows, start, end))).collect::<Vec<_>>().join(",")
}

/// What a live editor starts from.
pub enum LiveBase<'a> {
    /// The empty document, for a log with no stored revision.
    Empty,
    /// The document's stored file, for the first live session: fresh identities, the base rows
    /// and columns all participant 0's.
    File { bytes: &'a [u8] },
    /// A durable checkpoint with its identity tables, loaded under its stamp.
    Checkpoint { bytes: &'a [u8], session_data: &'a [u8], stamp: Stamp },
}

/// Why one of the person's own changes did nothing on every machine.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotAppliedReason {
    SheetDeleted,
    RowDeleted,
    ColumnDeleted,
    /// The target is not available for another reason, such as a formula reference that can no
    /// longer be written.
    TargetUnavailable,
    /// Undo or Redo left content that changed after the action.
    ChangedSinceUndo,
    /// The change is not valid on any machine.
    Invalid,
    /// The change could not be applied.
    Failed,
}

/// Something the host shows the person, in ShareCraft's frame.
#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveNotice {
    /// The person's change did nothing; a cell edit keeps the text they typed.
    NotApplied { seq: Seq, kept_text: Option<String>, reason: NotAppliedReason },
    /// The server refused the person's change.
    ChangeRefused { reason: RefusedReason },
    /// Undo or Redo left this many cells unchanged.
    UndoLeft { cells: usize },
    /// Undo or Redo was refused.
    UndoRefused { reason: String },
    /// A command that is not shared yet was refused.
    Unshared { command: String },
    /// New changes are paused at the unsaved-log limit.
    Paused { entries: u64, bytes: u64 },
    /// Changes may be made again.
    Resumed,
    /// The person's place had ended and they returned; this many changes made while away did
    /// nothing.
    Returned { not_applied: usize },
    /// The shown copy from before a return was archived.
    ArchivedReturn,
    /// Another person's changes made while away are arriving. (Sent once the room relays it.)
    OthersReturning { participant: ParticipantNo },
    /// The person's right to edit changed: `true` is read only.
    ReadOnly(bool),
    /// The document cannot be opened from the log.
    CannotOpen,
    /// This editor build does not match the session.
    Incompatible,
    /// The room refused this build. (Sent once the room relays it.)
    BuildRefused,
    /// The person was removed from the session.
    Removed,
}

/// Debug shows the length of kept text, never the text, so a notice in a log names no content.
impl fmt::Debug for LiveNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotApplied { seq, kept_text, reason } => f
                .debug_struct("NotApplied")
                .field("seq", seq)
                .field("kept_text_len", &kept_text.as_ref().map(String::len))
                .field("reason", reason)
                .finish(),
            Self::ChangeRefused { reason } => f.debug_struct("ChangeRefused").field("reason", reason).finish(),
            Self::UndoLeft { cells } => f.debug_struct("UndoLeft").field("cells", cells).finish(),
            Self::UndoRefused { reason } => f.debug_struct("UndoRefused").field("reason_len", &reason.len()).finish(),
            Self::Unshared { command } => f.debug_struct("Unshared").field("command", command).finish(),
            Self::Paused { entries, bytes } => f.debug_struct("Paused").field("entries", entries).field("bytes", bytes).finish(),
            Self::Resumed => f.write_str("Resumed"),
            Self::Returned { not_applied } => f.debug_struct("Returned").field("not_applied", not_applied).finish(),
            Self::ArchivedReturn => f.write_str("ArchivedReturn"),
            Self::OthersReturning { participant } => f.debug_struct("OthersReturning").field("participant", participant).finish(),
            Self::ReadOnly(read_only) => f.debug_tuple("ReadOnly").field(read_only).finish(),
            Self::CannotOpen => f.write_str("CannotOpen"),
            Self::Incompatible => f.write_str("Incompatible"),
            Self::BuildRefused => f.write_str("BuildRefused"),
            Self::Removed => f.write_str("Removed"),
        }
    }
}
