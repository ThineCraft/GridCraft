use collab_live::ids::LiveId;
use protocol::live::MAX_PAYLOAD_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::CoauthorError;

/// One command in the `gridcraft/1` payload format.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "id", deny_unknown_fields)]
pub enum SharedCommand {
    #[serde(rename = "cell.set")]
    CellSet {
        sheet: LiveId,
        cell: Json,
        input: String,
        #[serde(rename = "originYear")]
        origin_year: i32,
        references: Vec<Json>,
    },
    #[serde(rename = "edit.clearContents")]
    ClearContents { sheet: LiveId, range: Json },
    #[serde(rename = "home.numberFormat")]
    NumberFormat { sheet: LiveId, range: Json, value: Json },
    #[serde(rename = "home.fontName")]
    FontName { sheet: LiveId, range: Json, name: String },
    #[serde(rename = "home.fontSize")]
    FontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.fontColor")]
    FontColor { sheet: LiveId, range: Json, color: Json },
    #[serde(rename = "home.fillColor")]
    FillColor { sheet: LiveId, range: Json, color: Json },
    #[serde(rename = "home.borders")]
    Borders { sheet: LiveId, range: Json, value: Json },
    #[serde(rename = "home.alignTop")]
    AlignTop { sheet: LiveId, range: Json },
    #[serde(rename = "home.alignMiddle")]
    AlignMiddle { sheet: LiveId, range: Json },
    #[serde(rename = "home.alignBottom")]
    AlignBottom { sheet: LiveId, range: Json },
    #[serde(rename = "home.bold")]
    Bold { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.italic")]
    Italic { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.wrapText")]
    WrapText { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.alignLeft")]
    AlignLeft { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.alignCenter")]
    AlignCenter { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.alignRight")]
    AlignRight { sheet: LiveId, range: Json, on: bool },
    #[serde(rename = "home.underline")]
    Underline { sheet: LiveId, range: Json, on: bool, style: String },
    #[serde(rename = "home.doubleUnderline")]
    DoubleUnderline { sheet: LiveId, range: Json, on: bool, style: String },
    #[serde(rename = "home.increaseFontSize")]
    IncreaseFontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.decreaseFontSize")]
    DecreaseFontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.formatCells")]
    FormatCells { sheet: LiveId, range: Json, style: Json },
    #[serde(rename = "data.filterSet")]
    FilterSet { sheet: LiveId, range: Json },
    #[serde(rename = "data.filterRemove")]
    FilterRemove { sheet: LiveId, range: Json },
    #[serde(rename = "data.filterBy")]
    FilterBy { sheet: LiveId, range: Json, column: LiveId, filter: Json },
    #[serde(rename = "data.clearFilter")]
    ClearFilter { sheet: LiveId, range: Json },
    #[serde(rename = "data.reapply")]
    Reapply { sheet: LiveId, range: Json },
    #[serde(rename = "home.insertRows")]
    InsertRows { sheet: LiveId, rows: Json },
    #[serde(rename = "home.deleteRows")]
    DeleteRows { sheet: LiveId, rows: Json },
    #[serde(rename = "home.insertColumns")]
    InsertColumns { sheet: LiveId, cols: Json },
    #[serde(rename = "home.deleteColumns")]
    DeleteColumns { sheet: LiveId, cols: Json },
    #[serde(rename = "home.insertSheet")]
    InsertSheet { name: String, before: Json },
    #[serde(rename = "sheet.rename")]
    RenameSheet { sheet: LiveId, name: String },
    #[serde(rename = "home.deleteSheet")]
    DeleteSheet { sheet: LiveId },
}

/// A versioned command and the person's action number.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SharedEntry {
    pub version: u32,
    pub action: u64,
    pub command: SharedCommand,
}

/// Decodes one bounded, closed `gridcraft/1` entry.
pub fn decode_entry(payload: &str) -> Result<SharedEntry, CoauthorError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(CoauthorError::OversizedPayload { bytes: payload.len(), max: MAX_PAYLOAD_BYTES });
    }
    let value: Json = serde_json::from_str(payload).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let version = value.get("version").and_then(Json::as_u64).ok_or_else(|| malformed("missing or invalid `version`"))?;
    if version != 1 {
        return Err(CoauthorError::UnsupportedVersion(version));
    }
    let command = value.get("command").and_then(Json::as_object).ok_or_else(|| malformed("missing or invalid `command`"))?;
    let id = command.get("id").and_then(Json::as_str).ok_or_else(|| malformed("missing or invalid command `id`"))?;
    if !is_shared_command(id) {
        return Err(CoauthorError::UnsupportedCommand(id.to_string()));
    }
    for target in required_fields(id) {
        if !command.contains_key(*target) {
            return Err(CoauthorError::MissingTarget { command: id.to_string(), target: (*target).to_string() });
        }
    }
    serde_json::from_value(value).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

fn malformed(message: &str) -> CoauthorError {
    CoauthorError::MalformedPayload(message.to_string())
}

/// Whether a command may enter the first GridCraft live log.
pub fn is_shared_command(id: &str) -> bool {
    matches!(
        id,
        "cell.set"
            | "edit.clearContents"
            | "home.numberFormat"
            | "home.fontName"
            | "home.fontSize"
            | "home.fontColor"
            | "home.fillColor"
            | "home.borders"
            | "home.alignTop"
            | "home.alignMiddle"
            | "home.alignBottom"
            | "home.bold"
            | "home.italic"
            | "home.wrapText"
            | "home.alignLeft"
            | "home.alignCenter"
            | "home.alignRight"
            | "home.underline"
            | "home.doubleUnderline"
            | "home.increaseFontSize"
            | "home.decreaseFontSize"
            | "home.formatCells"
            | "data.filterSet"
            | "data.filterRemove"
            | "data.filterBy"
            | "data.clearFilter"
            | "data.reapply"
            | "home.insertRows"
            | "home.deleteRows"
            | "home.insertColumns"
            | "home.deleteColumns"
            | "home.insertSheet"
            | "sheet.rename"
            | "home.deleteSheet"
    )
}

fn required_fields(id: &str) -> &'static [&'static str] {
    match id {
        "cell.set" => &["sheet", "cell", "input", "originYear", "references"],
        "edit.clearContents" | "home.alignTop" | "home.alignMiddle" | "home.alignBottom" | "data.filterSet" | "data.filterRemove"
        | "data.clearFilter" | "data.reapply" => &["sheet", "range"],
        "home.bold" | "home.italic" | "home.wrapText" | "home.alignLeft" | "home.alignCenter" | "home.alignRight" => &["sheet", "range", "on"],
        "home.underline" | "home.doubleUnderline" => &["sheet", "range", "on", "style"],
        "home.increaseFontSize" | "home.decreaseFontSize" | "home.fontSize" => &["sheet", "range", "size"],
        "home.fontName" => &["sheet", "range", "name"],
        "home.fontColor" | "home.fillColor" => &["sheet", "range", "color"],
        "home.numberFormat" | "home.borders" => &["sheet", "range", "value"],
        "home.formatCells" => &["sheet", "range", "style"],
        "data.filterBy" => &["sheet", "range", "column", "filter"],
        "home.insertRows" | "home.deleteRows" => &["sheet", "rows"],
        "home.insertColumns" | "home.deleteColumns" => &["sheet", "cols"],
        "home.insertSheet" => &["name", "before"],
        "sheet.rename" => &["sheet", "name"],
        "home.deleteSheet" => &["sheet"],
        _ => &[],
    }
}
