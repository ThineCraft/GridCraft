use std::collections::{BTreeSet, HashSet};
use std::fmt;

use collab_live::ids::{IdError, LiveId};
use gridcraft_core::{CellError, CellRef, RangeRef};
use gridcraft_formula::{Anchor, Expr, RefKind, Reference, SheetSel};
use protocol::live::MAX_PAYLOAD_BYTES;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::{
    CellId, CoauthorError, GuardedPatch, IdentityDelete, IdentityInsert, IdentityPosition, IdentityRanges, IdentitySheetInsert, IdentityStructural,
    TargetGone, WorkbookIds,
};

const MAX_IDENTITY_TARGETS: usize = MAX_PAYLOAD_BYTES / 8;
// Each 3-D member carries both a sheet LiveId and a tagged identity reference;
// 64 bytes is below the shortest JSON representation, so this cap rejects only
// spans that cannot fit on the wire and does so before allocating for them.
const MIN_ENCODED_3D_MEMBER_BYTES: usize = 64;
const MAX_3D_SPAN_SHEETS: usize = MAX_PAYLOAD_BYTES / MIN_ENCODED_3D_MEMBER_BYTES;

/// One command in the `gridcraft/1` payload format.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "id")]
pub enum SharedCommand {
    #[serde(rename = "cell.set")]
    CellSet {
        sheet: LiveId,
        cell: CellId,
        input: String,
        #[serde(rename = "originYear")]
        origin_year: i32,
        formula: bool,
        references: Vec<IdentityReference>,
    },
    #[serde(rename = "edit.clearContents")]
    ClearContents { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "home.numberFormat")]
    NumberFormat {
        sheet: LiveId,
        range: IdentityRanges,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<NumberFormatName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    #[serde(rename = "home.fontName")]
    FontName { sheet: LiveId, range: IdentityRanges, name: String },
    #[serde(rename = "home.fontSize")]
    FontSize { sheet: LiveId, range: IdentityRanges, size: f64 },
    #[serde(rename = "home.fontColor")]
    FontColor { sheet: LiveId, range: IdentityRanges, color: CommandColor },
    #[serde(rename = "home.fillColor")]
    FillColor { sheet: LiveId, range: IdentityRanges, color: CommandColor },
    #[serde(rename = "home.borders")]
    Borders {
        sheet: LiveId,
        range: IdentityRanges,
        preset: BorderPreset,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<CommandBorderStyle>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<CommandColor>,
    },
    #[serde(rename = "home.alignTop")]
    AlignTop { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "home.alignMiddle")]
    AlignMiddle { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "home.alignBottom")]
    AlignBottom { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "home.bold")]
    Bold { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.italic")]
    Italic { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.wrapText")]
    WrapText { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.alignLeft")]
    AlignLeft { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.alignCenter")]
    AlignCenter { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.alignRight")]
    AlignRight { sheet: LiveId, range: IdentityRanges, on: bool },
    #[serde(rename = "home.underline")]
    Underline { sheet: LiveId, range: IdentityRanges, on: bool, style: UnderlineStyle },
    #[serde(rename = "home.doubleUnderline")]
    DoubleUnderline { sheet: LiveId, range: IdentityRanges, on: bool, style: UnderlineStyle },
    #[serde(rename = "home.increaseFontSize")]
    IncreaseFontSize { sheet: LiveId, range: IdentityRanges, size: f64 },
    #[serde(rename = "home.decreaseFontSize")]
    DecreaseFontSize { sheet: LiveId, range: IdentityRanges, size: f64 },
    #[serde(rename = "home.formatCells")]
    FormatCells { sheet: LiveId, range: IdentityRanges, style: StylePatch },
    #[serde(rename = "data.filterSet")]
    FilterSet { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "data.filterRemove")]
    FilterRemove { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "data.filterBy")]
    FilterBy {
        sheet: LiveId,
        range: IdentityRanges,
        column: LiveId,
        #[serde(flatten)]
        criteria: FilterCriteria,
    },
    #[serde(rename = "data.clearFilter")]
    ClearFilter { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "data.reapply")]
    Reapply { sheet: LiveId, range: IdentityRanges },
    #[serde(rename = "home.insertRows")]
    InsertRows { sheet: LiveId, rows: IdentityInsert },
    #[serde(rename = "home.deleteRows")]
    DeleteRows { sheet: LiveId, rows: IdentityDelete },
    #[serde(rename = "home.insertColumns")]
    InsertColumns { sheet: LiveId, cols: IdentityInsert },
    #[serde(rename = "home.deleteColumns")]
    DeleteColumns { sheet: LiveId, cols: IdentityDelete },
    #[serde(rename = "home.insertSheet")]
    InsertSheet { name: String, before: IdentitySheetInsert },
    #[serde(rename = "sheet.rename")]
    RenameSheet { sheet: LiveId, name: String },
    #[serde(rename = "home.deleteSheet")]
    DeleteSheet { sheet: LiveId },
    #[serde(rename = "edit.guardedPatch")]
    Guarded { patch: GuardedPatch },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum IdentitySheetSel {
    Current { sheet: LiveId },
    Named { sheet: LiveId },
    Span { start: LiveId, end: LiveId },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityAnchor {
    pub row: LiveId,
    pub col: LiveId,
    pub row_abs: bool,
    pub col_abs: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum IdentityRefKind {
    Cell { anchor: IdentityAnchor },
    Range { start: IdentityAnchor, end: IdentityAnchor },
    Rows { start: LiveId, start_abs: bool, end: LiveId, end_abs: bool },
    Columns { start: LiveId, start_abs: bool, end: LiveId, end_abs: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityReference {
    pub sheet: IdentitySheetSel,
    pub reference: IdentityRefKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub span_sheets: Vec<LiveId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub span_references: Vec<IdentityRefKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCommand {
    pub id: String,
    pub params: Json,
    pub structural: Option<IdentityStructural>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum NumberFormatName {
    General,
    Number,
    Currency,
    Accounting,
    #[serde(rename = "Short Date")]
    ShortDate,
    #[serde(rename = "Long Date")]
    LongDate,
    Time,
    Percentage,
    Fraction,
    Scientific,
    Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum CommandColor {
    Named(String),
    Theme(ThemeColor),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThemeColor {
    pub theme: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tint: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum BorderPreset {
    Bottom,
    Top,
    Left,
    Right,
    None,
    All,
    Outside,
    ThickOutside,
    ThickBottom,
    DoubleBottom,
    TopBottom,
    TopThickBottom,
    TopDoubleBottom,
    InsideHorizontal,
    InsideVertical,
    Inside,
    DiagonalDown,
    DiagonalUp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CommandBorderStyle {
    None,
    Thin,
    Medium,
    Thick,
    Dashed,
    Dotted,
    Double,
    Hair,
    MediumDashed,
    DashDot,
    MediumDashDot,
    DashDotDot,
    MediumDashDotDot,
    SlantDashDot,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum UnderlineStyle {
    None,
    Single,
    Double,
    SingleAccounting,
    DoubleAccounting,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StylePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<FontPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<FillPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub borders: Option<BordersPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<AlignmentPatch>,
    #[serde(rename = "numFmt", alias = "num_fmt", default, skip_serializing_if = "Option::is_none")]
    pub num_fmt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<ProtectionPatch>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FontPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<StyleUnderline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strike: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<StyleColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vert: Option<StyleVertAlign>,
    #[serde(default, skip_serializing_if = "PatchField::is_missing")]
    pub charset: PatchField<u8>,
    #[serde(default, skip_serializing_if = "PatchField::is_missing")]
    pub family: PatchField<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleUnderline {
    None,
    Single,
    Double,
    SingleAccounting,
    DoubleAccounting,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleVertAlign {
    Baseline,
    Superscript,
    Subscript,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleColor {
    Auto,
    Rgb(u32),
    Theme(u8, i16),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FillPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<StylePattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<StyleColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<StyleColor>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StylePattern {
    None,
    Solid,
    Gray50,
    Gray75,
    Gray25,
    Gray125,
    Gray0625,
    DarkHorizontal,
    DarkVertical,
    DarkDown,
    DarkUp,
    DarkGrid,
    DarkTrellis,
    LightHorizontal,
    LightVertical,
    LightDown,
    LightUp,
    LightGrid,
    LightTrellis,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BordersPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<BorderLinePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<BorderLinePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<BorderLinePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom: Option<BorderLinePatch>,
    #[serde(rename = "diagDown", alias = "diag_down", default, skip_serializing_if = "Option::is_none")]
    pub diag_down: Option<BorderLinePatch>,
    #[serde(rename = "diagUp", alias = "diag_up", default, skip_serializing_if = "Option::is_none")]
    pub diag_up: Option<BorderLinePatch>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BorderLinePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StyleBorderStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<StyleColor>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleBorderStyle {
    None,
    Thin,
    Medium,
    Thick,
    Dashed,
    Dotted,
    Double,
    Hair,
    MediumDashed,
    DashDot,
    MediumDashDot,
    DashDotDot,
    MediumDashDotDot,
    SlantDashDot,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AlignmentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<StyleHAlign>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<StyleVAlign>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shrink: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<i16>,
    #[serde(rename = "readingOrder", alias = "reading_order", default, skip_serializing_if = "PatchField::is_missing")]
    pub reading_order: PatchField<StyleReadingOrder>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum PatchField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> PatchField<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl<T: Serialize> Serialize for PatchField<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Missing | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| value.map_or(Self::Null, Self::Value))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleHAlign {
    General,
    Left,
    Center,
    Right,
    Fill,
    Justify,
    CenterAcross,
    Distributed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleVAlign {
    Top,
    Center,
    Bottom,
    Justify,
    Distributed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleReadingOrder {
    Context,
    LeftToRight,
    RightToLeft,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProtectionPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilterCriteria {
    Values(FilterValues),
    Custom(FilterCustom),
    Top(FilterTop),
    AboveAverage(FilterAboveAverage),
    Clear(FilterClear),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterValues {
    pub values: Vec<FilterValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blanks: Option<bool>,
}

pub type FilterValue = Json;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterCustom {
    pub custom: CustomFilter,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<FilterValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value2: Option<FilterValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub and: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterTop {
    pub top: TopFilter,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TopFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterAboveAverage {
    #[serde(rename = "aboveAverage")]
    pub above_average: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterClear {
    pub clear: bool,
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
    reject_duplicate_keys(payload)?;
    let value: Json = serde_json::from_str(payload).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let version = value.get("version").and_then(Json::as_u64).ok_or_else(|| malformed("missing or invalid `version`"))?;
    if version != 1 {
        return Err(CoauthorError::UnsupportedVersion(version));
    }
    let command = value.get("command").and_then(Json::as_object).ok_or_else(|| malformed("missing or invalid `command`"))?;
    let id = command.get("id").and_then(Json::as_str).ok_or_else(|| malformed("missing or invalid command `id`"))?;
    if !is_wire_command(id) {
        return Err(CoauthorError::UnsupportedCommand(id.to_string()));
    }
    if let Some(field) = unexpected_command_field(id, command) {
        return Err(malformed(&format!("unknown field `{field}` in command `{id}`")));
    }
    if let Some(target) = missing_required_field(id, command) {
        return Err(CoauthorError::MissingTarget { command: id.to_string(), target: target.to_string() });
    }
    serde_json::from_value(value).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

fn reject_duplicate_keys(payload: &str) -> Result<(), CoauthorError> {
    let mut deserializer = serde_json::Deserializer::from_str(payload);
    DuplicateFree { depth: 0 }
        .deserialize(&mut deserializer)
        .and_then(|()| deserializer.end())
        .map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

struct DuplicateFree {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for DuplicateFree {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const MAX_JSON_DEPTH: usize = 128;
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom("JSON nesting exceeds 128 levels"));
        }
        deserializer.deserialize_any(DuplicateFreeVisitor { depth: self.depth })
    }
}

struct DuplicateFreeVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for DuplicateFreeVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element_seed(DuplicateFree { depth: self.depth + 1 })?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate key `{key}`")));
            }
            map.next_value_seed(DuplicateFree { depth: self.depth + 1 })?;
        }
        Ok(())
    }
}

fn unexpected_command_field<'a>(id: &str, command: &'a serde_json::Map<String, Json>) -> Option<&'a str> {
    let allowed: &[&str] = match id {
        "cell.set" => &["id", "sheet", "cell", "input", "originYear", "formula", "references"],
        "edit.clearContents" | "home.alignTop" | "home.alignMiddle" | "home.alignBottom" | "data.filterSet" | "data.filterRemove"
        | "data.clearFilter" | "data.reapply" => &["id", "sheet", "range"],
        "home.numberFormat" => &["id", "sheet", "range", "format", "code"],
        "home.fontName" => &["id", "sheet", "range", "name"],
        "home.fontSize" | "home.increaseFontSize" | "home.decreaseFontSize" => &["id", "sheet", "range", "size"],
        "home.fontColor" | "home.fillColor" => &["id", "sheet", "range", "color"],
        "home.borders" => &["id", "sheet", "range", "preset", "style", "color"],
        "home.bold" | "home.italic" | "home.wrapText" | "home.alignLeft" | "home.alignCenter" | "home.alignRight" => &["id", "sheet", "range", "on"],
        "home.underline" | "home.doubleUnderline" => &["id", "sheet", "range", "on", "style"],
        "home.formatCells" => &["id", "sheet", "range", "style"],
        "data.filterBy" => &["id", "sheet", "range", "column", "values", "blanks", "custom", "top", "aboveAverage", "clear"],
        "home.insertRows" | "home.deleteRows" => &["id", "sheet", "rows"],
        "home.insertColumns" | "home.deleteColumns" => &["id", "sheet", "cols"],
        "home.insertSheet" => &["id", "name", "before"],
        "sheet.rename" => &["id", "sheet", "name"],
        "home.deleteSheet" => &["id", "sheet"],
        "edit.guardedPatch" => &["id", "patch"],
        _ => &[],
    };
    command.keys().find(|field| !allowed.contains(&field.as_str())).map(String::as_str)
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

fn is_wire_command(id: &str) -> bool {
    is_shared_command(id) || id == "edit.guardedPatch"
}

fn missing_required_field(id: &str, command: &serde_json::Map<String, Json>) -> Option<&'static str> {
    let fields: &[&str] = match id {
        "cell.set" => &["sheet", "cell", "input", "originYear", "formula", "references"],
        "edit.clearContents" | "home.alignTop" | "home.alignMiddle" | "home.alignBottom" | "data.filterSet" | "data.filterRemove"
        | "data.clearFilter" | "data.reapply" => &["sheet", "range"],
        "home.bold" | "home.italic" | "home.wrapText" | "home.alignLeft" | "home.alignCenter" | "home.alignRight" => &["sheet", "range", "on"],
        "home.underline" | "home.doubleUnderline" => &["sheet", "range", "on", "style"],
        "home.increaseFontSize" | "home.decreaseFontSize" | "home.fontSize" => &["sheet", "range", "size"],
        "home.fontName" => &["sheet", "range", "name"],
        "home.fontColor" | "home.fillColor" => &["sheet", "range", "color"],
        "home.numberFormat" => &["sheet", "range"],
        "home.borders" => &["sheet", "range", "preset"],
        "home.formatCells" => &["sheet", "range", "style"],
        "data.filterBy" => &["sheet", "range", "column"],
        "home.insertRows" | "home.deleteRows" => &["sheet", "rows"],
        "home.insertColumns" | "home.deleteColumns" => &["sheet", "cols"],
        "home.insertSheet" => &["name", "before"],
        "sheet.rename" => &["sheet", "name"],
        "home.deleteSheet" => &["sheet"],
        "edit.guardedPatch" => &["patch"],
        _ => &[],
    };
    if let Some(field) = fields.iter().find(|field| !command.contains_key(**field)) {
        return Some(field);
    }
    match id {
        "home.numberFormat" if !command.contains_key("format") && !command.contains_key("code") => Some("format or code"),
        "data.filterBy" if !["values", "custom", "top", "aboveAverage", "clear"].iter().any(|field| command.contains_key(*field)) => {
            Some("values|custom|top|aboveAverage|clear")
        }
        _ => None,
    }
}

pub fn encode_command(
    ids: &WorkbookIds,
    session: &crate::Session,
    action: u64,
    id: &str,
    params: &Json,
    origin_year: i32,
    allocate: &mut dyn FnMut(u32) -> Result<LiveId, IdError>,
) -> Result<SharedEntry, CoauthorError> {
    if id == "edit.guardedPatch" {
        return Err(CoauthorError::UnsupportedCommand(id.to_string()));
    }
    if !is_shared_command(id) {
        return Err(CoauthorError::UnsupportedCommand(id.to_string()));
    }
    let mut command = serde_json::Map::new();
    command.insert("id".into(), Json::String(id.to_string()));
    match id {
        "home.insertSheet" => {
            copy_required(params, &mut command, "name", id)?;
            let workbook = &session.doc()?.wb;
            let at = params
                .get("before")
                .and_then(Json::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or_else(|| workbook.active_sheet().saturating_add(1))
                .min(workbook.sheets.len());
            let before = if at == workbook.sheets.len() { IdentityPosition::End } else { IdentityPosition::Before(ids.sheet_id(at)?) };
            let target = IdentitySheetInsert { before, id: allocate(1)? };
            command.insert("before".into(), to_json(target)?);
        }
        "home.insertRows" | "home.insertColumns" => {
            let sheet_index = crate::cmd::target_sheet(session, params)?;
            let sheet = ids.sheet_id(sheet_index)?;
            command.insert("sheet".into(), to_json(sheet)?);
            let key = if id == "home.insertRows" { "rows" } else { "cols" };
            let axis = if id == "home.insertRows" { AxisParam::Rows } else { AxisParam::Columns };
            let (at, count) = line_span(session, params, key, axis)?;
            let limit = if axis == AxisParam::Rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
            if count == 0 || at.checked_add(count).is_none_or(|end| end > limit) {
                return Err(CoauthorError::MalformedPayload(format!("insert span {at}..+{count} is outside a worksheet axis of {limit}")));
            }
            let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let ordered = if axis == AxisParam::Rows { &axes.rows } else { &axes.cols };
            let before = ordered.id_at(at).map_or(IdentityPosition::End, IdentityPosition::Before);
            let insert = IdentityInsert { before, first: allocate(count)?, count };
            command.insert(key.into(), to_json(insert)?);
        }
        "home.deleteRows" | "home.deleteColumns" => {
            let sheet_index = crate::cmd::target_sheet(session, params)?;
            let sheet = ids.sheet_id(sheet_index)?;
            command.insert("sheet".into(), to_json(sheet)?);
            let key = if id == "home.deleteRows" { "rows" } else { "cols" };
            let axis = if id == "home.deleteRows" { AxisParam::Rows } else { AxisParam::Columns };
            let positions = deletion_positions(params, key, axis)?;
            if positions.len() > MAX_IDENTITY_TARGETS {
                return Err(CoauthorError::OversizedPayload { bytes: positions.len().saturating_mul(8), max: MAX_PAYLOAD_BYTES });
            }
            let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
            let ordered = if axis == AxisParam::Rows { &axes.rows } else { &axes.cols };
            let mut targets = Vec::with_capacity(positions.len());
            for position in positions {
                targets.push(ordered.id_at(position).ok_or_else(|| CoauthorError::UnknownIdentity(format!("{key} position {position}")))?);
            }
            let first = allocate(
                u32::try_from(targets.len())
                    .map_err(|_| CoauthorError::OversizedPayload { bytes: targets.len().saturating_mul(8), max: MAX_PAYLOAD_BYTES })?,
            )?;
            command.insert(key.into(), to_json(IdentityDelete { ids: targets, first })?);
        }
        _ => {
            let sheet_index = crate::cmd::target_sheet(session, params)?;
            let sheet = ids.sheet_id(sheet_index)?;
            command.insert("sheet".into(), to_json(sheet)?);
            if id == "cell.set" {
                let cell_ref =
                    crate::cmd::cell_param_on(session, params, "cell", sheet_index)?.unwrap_or(crate::cmd::target_range(session, params)?.start);
                let cell = ids.cell_id(sheet_index, cell_ref)?;
                let input = params
                    .get("input")
                    .and_then(Json::as_str)
                    .ok_or_else(|| CoauthorError::MissingTarget { command: id.to_string(), target: "input".into() })?;
                let (input, formula, references) = encode_formula(ids, session, sheet_index, cell_ref, input)?;
                command.insert("cell".into(), to_json(cell)?);
                command.insert("input".into(), Json::String(input));
                command.insert("originYear".into(), Json::from(origin_year));
                command.insert("formula".into(), Json::from(formula));
                command.insert("references".into(), to_json(references)?);
            } else if id == "sheet.rename" {
                copy_required(params, &mut command, "name", id)?;
            } else if id != "home.deleteSheet" {
                if params.get("range").and_then(Json::as_str).is_some_and(|value| value.len() > MAX_PAYLOAD_BYTES) {
                    return Err(CoauthorError::OversizedPayload { bytes: params["range"].as_str().map_or(0, str::len), max: MAX_PAYLOAD_BYTES });
                }
                let ranges = crate::cmd::target_ranges(session, params)?;
                let identity_ranges = ranges.iter().copied().map(|range| ids.range_id(sheet_index, range)).collect::<Result<Vec<_>, _>>()?;
                command.insert("range".into(), to_json(IdentityRanges::from_vec(identity_ranges)?)?);
                if id == "data.filterBy" {
                    if ranges.len() != 1 {
                        return Err(CoauthorError::MalformedPayload("filter command requires exactly one range".into()));
                    }
                    let range =
                        ranges.first().copied().ok_or_else(|| CoauthorError::MalformedPayload("filter command requires exactly one range".into()))?;
                    let column = filter_column(params, range)?;
                    let axes = ids.by_id.get(&sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {sheet:?}")))?;
                    let column = axes.cols.id_at(column).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column position {column}")))?;
                    command.insert("column".into(), to_json(column)?);
                }
                copy_command_values(id, params, &mut command)?;
            }
        }
    }
    if let Some(field) = unexpected_command_field(id, &command) {
        return Err(CoauthorError::MalformedPayload(format!("unknown encoded field `{field}` in command `{id}`")));
    }
    let command = serde_json::from_value(Json::Object(command)).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
    let entry = SharedEntry { version: 1, action, command };
    let bytes = serde_json::to_vec(&entry).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?.len();
    if bytes > MAX_PAYLOAD_BYTES {
        return Err(CoauthorError::OversizedPayload { bytes, max: MAX_PAYLOAD_BYTES });
    }
    Ok(entry)
}

pub(crate) fn guarded_entry(action: u64, patch: GuardedPatch) -> SharedEntry {
    SharedEntry { version: 1, action, command: SharedCommand::Guarded { patch } }
}

/// Whether every identity this command allocates belongs to `participant`, the sender of the
/// ordered entry. Identities are `(participant, counter)` pairs: an entry that names another
/// participant's identities as its new rows, columns or sheet could reserve that person's future
/// counters and make their later structural changes fail on every client. Guarded patches only
/// move identities that already exist, so they allocate none.
pub fn allocates_only_own_identities(command: &SharedCommand, participant: u32) -> bool {
    match command {
        SharedCommand::InsertRows { rows: insert, .. } | SharedCommand::InsertColumns { cols: insert, .. } => insert.first.0 == participant,
        SharedCommand::DeleteRows { rows: delete, .. } | SharedCommand::DeleteColumns { cols: delete, .. } => delete.first.0 == participant,
        SharedCommand::InsertSheet { before, .. } => before.id.0 == participant,
        _ => true,
    }
}

pub fn resolve_command(ids: &WorkbookIds, session: &crate::Session, entry: &SharedEntry) -> Result<ResolvedCommand, TargetGone> {
    let mut value = serde_json::to_value(&entry.command).map_err(|error| TargetGone::Malformed(error.to_string()))?;
    let object = value.as_object_mut().ok_or_else(|| TargetGone::Malformed("command did not serialize as an object".into()))?;
    let id = object
        .remove("id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| TargetGone::Malformed("command id is absent".into()))?;
    let mut structural = None;

    match &entry.command {
        SharedCommand::CellSet { sheet: declared_sheet, cell, input, formula, references, .. } => {
            if *declared_sheet != cell.sheet {
                return Err(TargetGone::Malformed("cell target names a different sheet".into()));
            }
            let (sheet, cell, resolved_input) = resolve_cell_input(ids, session, *cell, input, *formula, references)?;
            object.insert("sheet".into(), Json::from(sheet));
            object.insert("cell".into(), Json::String(cell.a1()));
            object.insert("input".into(), Json::String(resolved_input));
            object.remove("formula");
            object.remove("references");
        }
        SharedCommand::InsertRows { sheet, rows } => {
            let sheet_index = ids.resolve_sheet(*sheet)?;
            let at = resolve_position(ids, *sheet, rows.before, AxisParam::Rows)?;
            validate_insert_span(at, rows.count, gridcraft_core::MAX_ROWS)?;
            object.insert("sheet".into(), Json::from(sheet_index));
            object.insert("rows".into(), Json::String(line_text(at, rows.count, AxisParam::Rows)?));
            structural = Some(IdentityStructural::InsertRows { sheet: *sheet, before: rows.before, first: rows.first, count: rows.count });
        }
        SharedCommand::DeleteRows { sheet, rows } => {
            let sheet_index = ids.resolve_sheet(*sheet)?;
            object.insert("sheet".into(), Json::from(sheet_index));
            object.insert("rows".into(), Json::String(resolve_delete_lines(ids, *sheet, &rows.ids, AxisParam::Rows)?));
            structural = Some(IdentityStructural::DeleteRows { sheet: *sheet, ids: rows.ids.clone(), first: rows.first });
        }
        SharedCommand::InsertColumns { sheet, cols } => {
            let sheet_index = ids.resolve_sheet(*sheet)?;
            let at = resolve_position(ids, *sheet, cols.before, AxisParam::Columns)?;
            validate_insert_span(at, cols.count, gridcraft_core::MAX_COLS)?;
            object.insert("sheet".into(), Json::from(sheet_index));
            object.insert("cols".into(), Json::String(line_text(at, cols.count, AxisParam::Columns)?));
            structural = Some(IdentityStructural::InsertColumns { sheet: *sheet, before: cols.before, first: cols.first, count: cols.count });
        }
        SharedCommand::DeleteColumns { sheet, cols } => {
            let sheet_index = ids.resolve_sheet(*sheet)?;
            object.insert("sheet".into(), Json::from(sheet_index));
            object.insert("cols".into(), Json::String(resolve_delete_lines(ids, *sheet, &cols.ids, AxisParam::Columns)?));
            structural = Some(IdentityStructural::DeleteColumns { sheet: *sheet, ids: cols.ids.clone(), first: cols.first });
        }
        SharedCommand::InsertSheet { before, .. } => {
            let at = match before.before {
                IdentityPosition::End => ids.sheets.len(),
                IdentityPosition::Before(target) => match ids.sheets.find(target) {
                    collab_live::ids::Lookup::At(at) => at,
                    collab_live::ids::Lookup::Deleted { .. } => return Err(TargetGone::DeletedSheet(target)),
                    collab_live::ids::Lookup::Unknown => return Err(TargetGone::UnknownSheet(target)),
                },
            };
            object.insert("before".into(), Json::from(at));
            structural = Some(IdentityStructural::InsertSheet { before: before.before, id: before.id });
        }
        SharedCommand::DeleteSheet { sheet } => {
            object.insert("sheet".into(), Json::from(ids.resolve_sheet(*sheet)?));
            structural = Some(IdentityStructural::DeleteSheet { id: *sheet });
        }
        SharedCommand::Guarded { .. } => return Err(TargetGone::Malformed("guarded patch is resolved by the co-author editor".into())),
        _ => {
            resolve_common_targets(ids, object)?;
        }
    }
    if let Some(change) = structural.as_ref() {
        ids.validate_structural(change).map_err(|error| TargetGone::Malformed(format!("invalid structural identity change: {error}")))?;
    }
    Ok(ResolvedCommand { id, params: Json::Object(object.clone()), structural })
}

pub(crate) fn resolve_cell_input(
    ids: &WorkbookIds,
    session: &crate::Session,
    target: CellId,
    input: &str,
    formula: bool,
    references: &[IdentityReference],
) -> Result<(usize, CellRef, String), TargetGone> {
    let (sheet, cell) = ids.resolve_cell(target)?;
    let resolved = if formula {
        resolve_formula(ids, session, target.sheet, input, references)?
    } else {
        if !references.is_empty() {
            return Err(TargetGone::Malformed("literal input carries formula references".into()));
        }
        let document = session.doc().map_err(|error| TargetGone::Malformed(error.to_string()))?;
        let text_formatted =
            document.wb.sheet(sheet).is_some_and(|target_sheet| document.wb.styles.get(target_sheet.style_id(cell)).num_fmt.as_str() == "@");
        if crate::cmd::edit::is_formula_input(input) && !text_formatted {
            return Err(TargetGone::Malformed("formula-like input is marked as a literal outside a text-formatted cell".into()));
        }
        input.to_string()
    };
    Ok((sheet, cell, resolved))
}

fn resolve_common_targets(ids: &WorkbookIds, object: &mut serde_json::Map<String, Json>) -> Result<(), TargetGone> {
    let declared_sheet = object
        .get("sheet")
        .cloned()
        .map(|value| serde_json::from_value(value).map_err(|error| TargetGone::Malformed(error.to_string())))
        .transpose()?;
    if let Some(sheet) = declared_sheet {
        object.insert("sheet".into(), Json::from(ids.resolve_sheet(sheet)?));
    }
    if let Some(range_value) = object.get("range").cloned() {
        let ranges: IdentityRanges = serde_json::from_value(range_value).map_err(|error| TargetGone::Malformed(error.to_string()))?;
        if ranges.as_slice().is_empty() {
            return Err(TargetGone::Malformed("range target is empty".into()));
        }
        let mut resolved = Vec::with_capacity(ranges.as_slice().len());
        for range in ranges.as_slice() {
            if declared_sheet != Some(range.sheet) {
                return Err(TargetGone::Malformed("range target names a different sheet".into()));
            }
            resolved.push(ids.resolve_range(*range)?.1.a1());
        }
        object.insert("range".into(), Json::String(resolved.join(",")));
    }
    if let Some(column_value) = object.get("column").cloned() {
        let column: LiveId = serde_json::from_value(column_value).map_err(|error| TargetGone::Malformed(error.to_string()))?;
        let sheet = declared_sheet.ok_or_else(|| TargetGone::Malformed("filter column has no resolved sheet".into()))?;
        let axes = ids.by_id.get(&sheet).ok_or(TargetGone::UnknownSheet(sheet))?;
        let position = match axes.cols.find(column) {
            collab_live::ids::Lookup::At(position) => position,
            collab_live::ids::Lookup::Deleted { .. } => return Err(TargetGone::DeletedColumn(column)),
            collab_live::ids::Lookup::Unknown => return Err(TargetGone::UnknownColumn(column)),
        };
        let range = object
            .get("range")
            .and_then(Json::as_str)
            .and_then(RangeRef::parse)
            .ok_or_else(|| TargetGone::Malformed("filter range is absent".into()))?;
        if position < range.start.col || position > range.end.col {
            return Err(TargetGone::Malformed("filter column is outside its range".into()));
        }
        object.insert("column".into(), Json::from(position - range.start.col));
    }
    Ok(())
}

fn copy_command_values(id: &str, params: &Json, command: &mut serde_json::Map<String, Json>) -> Result<(), CoauthorError> {
    let keys: &[&str] = match id {
        "home.numberFormat" => &["format", "code"],
        "home.fontName" => &["name"],
        "home.fontSize" | "home.increaseFontSize" | "home.decreaseFontSize" => &["size"],
        "home.fontColor" | "home.fillColor" => &["color"],
        "home.borders" => &["preset", "style", "color"],
        "home.bold" | "home.italic" | "home.wrapText" | "home.alignLeft" | "home.alignCenter" | "home.alignRight" => &["on"],
        "home.underline" | "home.doubleUnderline" => &["on", "style"],
        "home.formatCells" => &["style"],
        "data.filterBy" => &["values", "blanks", "custom", "top", "aboveAverage", "clear"],
        _ => &[],
    };
    for key in keys {
        if let Some(value) = params.get(*key) {
            command.insert((*key).to_string(), value.clone());
        }
    }
    if let Some(target) = missing_required_field(id, command) {
        return Err(CoauthorError::MissingTarget { command: id.to_string(), target: target.to_string() });
    }
    Ok(())
}

fn copy_required(params: &Json, output: &mut serde_json::Map<String, Json>, key: &str, id: &str) -> Result<(), CoauthorError> {
    let value = params.get(key).cloned().ok_or_else(|| CoauthorError::MissingTarget { command: id.to_string(), target: key.to_string() })?;
    output.insert(key.to_string(), value);
    Ok(())
}

fn to_json(value: impl Serialize) -> Result<Json, CoauthorError> {
    serde_json::to_value(value).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AxisParam {
    Rows,
    Columns,
}

impl AxisParam {
    fn bounds(self, range: RangeRef) -> (u32, u32) {
        match self {
            Self::Rows => (range.start.row, range.end.row),
            Self::Columns => (range.start.col, range.end.col),
        }
    }
}

fn line_span(session: &crate::Session, params: &Json, key: &str, axis: AxisParam) -> Result<(u32, u32), CoauthorError> {
    let range = params
        .get(key)
        .and_then(Json::as_str)
        .and_then(crate::cmd::parse_lines)
        .or_else(|| crate::cmd::target_range(session, params).ok())
        .ok_or_else(|| CoauthorError::MissingTarget { command: key.to_string(), target: key.to_string() })?;
    let (start, end) = axis.bounds(range);
    let count = params
        .get("count")
        .and_then(Json::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| end.saturating_sub(start).saturating_add(1))
        .max(1);
    Ok((start, count))
}

fn deletion_positions(params: &Json, key: &str, axis: AxisParam) -> Result<Vec<u32>, CoauthorError> {
    let text =
        params.get(key).and_then(Json::as_str).ok_or_else(|| CoauthorError::MissingTarget { command: key.to_string(), target: key.to_string() })?;
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(CoauthorError::OversizedPayload { bytes: text.len(), max: MAX_PAYLOAD_BYTES });
    }
    let limit = if axis == AxisParam::Rows { gridcraft_core::MAX_ROWS } else { gridcraft_core::MAX_COLS };
    let mut positions = BTreeSet::new();
    if let Some(count) = params.get("count").and_then(Json::as_u64).and_then(|value| u32::try_from(value).ok()) {
        let range = crate::cmd::parse_lines(text).ok_or_else(|| CoauthorError::MalformedPayload(format!("invalid {key} target `{text}`")))?;
        let start = axis.bounds(range).0;
        if count == 0 {
            return Err(CoauthorError::MalformedPayload(format!("{key} deletion count must be positive")));
        }
        let count = count.min(limit.saturating_sub(start));
        if usize::try_from(count).unwrap_or(usize::MAX) > MAX_IDENTITY_TARGETS {
            return Err(CoauthorError::OversizedPayload {
                bytes: usize::try_from(count).unwrap_or(usize::MAX).saturating_mul(8),
                max: MAX_PAYLOAD_BYTES,
            });
        }
        for position in start..start.saturating_add(count) {
            positions.insert(position);
        }
    } else {
        for part in text.split(',') {
            let range = crate::cmd::parse_lines(part).ok_or_else(|| CoauthorError::MalformedPayload(format!("invalid {key} target `{part}`")))?;
            let (start, end) = axis.bounds(range);
            for position in start..=end {
                positions.insert(position);
                if positions.len() > MAX_IDENTITY_TARGETS {
                    return Err(CoauthorError::OversizedPayload { bytes: positions.len().saturating_mul(8), max: MAX_PAYLOAD_BYTES });
                }
            }
        }
    }
    if positions.is_empty() {
        return Err(CoauthorError::MalformedPayload(format!("{key} target is empty")));
    }
    Ok(positions.into_iter().collect())
}

fn validate_insert_span(at: u32, count: u32, limit: u32) -> Result<(), TargetGone> {
    if count == 0 || at.checked_add(count).is_none_or(|end| end > limit) {
        return Err(TargetGone::Malformed(format!("insert span {at}..+{count} is outside a worksheet axis of {limit}")));
    }
    Ok(())
}

fn filter_column(params: &Json, range: RangeRef) -> Result<u32, CoauthorError> {
    match params.get("column") {
        Some(Json::String(value)) => gridcraft_core::letters_to_col(value.trim_start_matches('$'))
            .filter(|column| *column >= range.start.col && *column <= range.end.col)
            .ok_or_else(|| CoauthorError::MalformedPayload("filter column is outside the range".into())),
        Some(Json::Number(value)) => value
            .as_u64()
            .and_then(|offset| u32::try_from(offset).ok())
            .and_then(|offset| range.start.col.checked_add(offset))
            .filter(|column| *column <= range.end.col)
            .ok_or_else(|| CoauthorError::MalformedPayload("filter column is outside the range".into())),
        _ => Err(CoauthorError::MissingTarget { command: "data.filterBy".into(), target: "column".into() }),
    }
}

fn encode_formula(
    ids: &WorkbookIds,
    session: &crate::Session,
    host_sheet: usize,
    target: CellRef,
    input: &str,
) -> Result<(String, bool, Vec<IdentityReference>), CoauthorError> {
    encode_formula_in_workbook(ids, &session.doc()?.wb, host_sheet, target, input, false)
}

pub(crate) fn encode_formula_in_workbook(
    ids: &WorkbookIds,
    workbook: &gridcraft_model::Workbook,
    host_sheet: usize,
    target: CellRef,
    input: &str,
    stored_formula: bool,
) -> Result<(String, bool, Vec<IdentityReference>), CoauthorError> {
    let text_formatted = workbook.sheet(host_sheet).is_some_and(|sheet| workbook.styles.get(sheet.style_id(target)).num_fmt.as_str() == "@");
    if (!stored_formula && text_formatted) || !crate::cmd::edit::is_formula_input(input) {
        return Ok((input.to_string(), false, Vec::new()));
    }
    let body = input.strip_prefix('=').unwrap_or(input);
    let expr = gridcraft_formula::parse(body).map_err(|error| CoauthorError::MalformedPayload(format!("invalid formula: {error}")))?;
    let mut references = Vec::new();
    for reference in expr.references() {
        references.push(identity_reference(ids, workbook, host_sheet, reference)?);
    }
    Ok((format!("={}", gridcraft_formula::print(&expr)), true, references))
}

fn identity_reference(
    ids: &WorkbookIds,
    workbook: &gridcraft_model::Workbook,
    host_sheet: usize,
    reference: &Reference,
) -> Result<IdentityReference, CoauthorError> {
    let (sheet, target_index, span_bounds) = match &reference.sheet {
        SheetSel::Current => (IdentitySheetSel::Current { sheet: ids.sheet_id(host_sheet)? }, host_sheet, None),
        SheetSel::Named(name) => {
            let index = workbook.sheet_index(name).ok_or_else(|| CoauthorError::MalformedPayload(format!("formula names no sheet `{name}`")))?;
            (IdentitySheetSel::Named { sheet: ids.sheet_id(index)? }, index, None)
        }
        SheetSel::Span(start, end) => {
            let start_index =
                workbook.sheet_index(start).ok_or_else(|| CoauthorError::MalformedPayload(format!("formula names no sheet `{start}`")))?;
            let end_index = workbook.sheet_index(end).ok_or_else(|| CoauthorError::MalformedPayload(format!("formula names no sheet `{end}`")))?;
            let low = start_index.min(end_index);
            let high = start_index.max(end_index);
            let span_len = high
                .checked_sub(low)
                .and_then(|len| len.checked_add(1))
                .ok_or(CoauthorError::OversizedPayload { bytes: usize::MAX, max: MAX_PAYLOAD_BYTES })?;
            if span_len > MAX_3D_SPAN_SHEETS {
                return Err(CoauthorError::OversizedPayload { bytes: span_len.saturating_mul(MIN_ENCODED_3D_MEMBER_BYTES), max: MAX_PAYLOAD_BYTES });
            }
            (IdentitySheetSel::Span { start: ids.sheet_id(start_index)?, end: ids.sheet_id(end_index)? }, start_index, Some((low, high)))
        }
    };
    let encoded_reference = identity_ref_kind(ids, target_index, &reference.kind)?;
    let span_capacity = span_bounds.map_or(0, |(low, high)| high - low + 1);
    let mut span_sheets = Vec::with_capacity(span_capacity);
    let mut span_references = Vec::with_capacity(span_capacity);
    if let Some((low, high)) = span_bounds {
        for index in low..=high {
            span_sheets.push(ids.sheet_id(index)?);
            span_references.push(identity_ref_kind(ids, index, &reference.kind)?);
        }
    }
    Ok(IdentityReference { sheet, reference: encoded_reference, span_sheets, span_references })
}

fn identity_ref_kind(ids: &WorkbookIds, target_index: usize, reference: &RefKind) -> Result<IdentityRefKind, CoauthorError> {
    let target_sheet = ids.sheet_id(target_index)?;
    let encoded = match reference {
        RefKind::Cell(anchor) => IdentityRefKind::Cell { anchor: identity_anchor(ids, target_index, *anchor)? },
        RefKind::Range(start, end) => {
            IdentityRefKind::Range { start: identity_anchor(ids, target_index, *start)?, end: identity_anchor(ids, target_index, *end)? }
        }
        RefKind::Rows(start, start_abs, end, end_abs) => {
            let axes = ids.by_id.get(&target_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {target_sheet:?}")))?;
            IdentityRefKind::Rows {
                start: axes.rows.id_at(*start).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row {start}")))?,
                start_abs: *start_abs,
                end: axes.rows.id_at(*end).ok_or_else(|| CoauthorError::UnknownIdentity(format!("row {end}")))?,
                end_abs: *end_abs,
            }
        }
        RefKind::Cols(start, start_abs, end, end_abs) => {
            let axes = ids.by_id.get(&target_sheet).ok_or_else(|| CoauthorError::UnknownIdentity(format!("sheet {target_sheet:?}")))?;
            IdentityRefKind::Columns {
                start: axes.cols.id_at(*start).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column {start}")))?,
                start_abs: *start_abs,
                end: axes.cols.id_at(*end).ok_or_else(|| CoauthorError::UnknownIdentity(format!("column {end}")))?,
                end_abs: *end_abs,
            }
        }
    };
    Ok(encoded)
}

fn identity_anchor(ids: &WorkbookIds, sheet: usize, anchor: Anchor) -> Result<IdentityAnchor, CoauthorError> {
    let cell = ids.cell_id(sheet, anchor.cell())?;
    Ok(IdentityAnchor { row: cell.row, col: cell.col, row_abs: anchor.row_abs, col_abs: anchor.col_abs })
}

fn resolve_formula(
    ids: &WorkbookIds,
    session: &crate::Session,
    host_sheet: LiveId,
    input: &str,
    references: &[IdentityReference],
) -> Result<String, TargetGone> {
    let Some(body) = input.strip_prefix('=') else {
        return Err(TargetGone::Malformed("input marked as a formula is not a canonical formula".into()));
    };
    let expr = gridcraft_formula::parse(body).map_err(|error| TargetGone::Malformed(format!("invalid canonical formula: {error}")))?;
    if expr.references().len() != references.len() {
        return Err(TargetGone::Malformed("formula reference count does not match its identity references".into()));
    }
    let workbook = &session.doc().map_err(|error| TargetGone::Malformed(error.to_string()))?.wb;
    let mut replacements = Vec::with_capacity(references.len());
    for reference in references {
        replacements.push(resolve_identity_reference(ids, workbook, host_sheet, reference));
    }
    let mut replacements = replacements.into_iter();
    let mut failure = None;
    let resolved = expr.map(&mut |node| {
        if !matches!(node, Expr::Ref(_)) {
            return node;
        }
        match replacements.next() {
            Some(Ok(reference)) => Expr::Ref(reference),
            Some(Err(error)) if is_deleted(&error) => Expr::Error(CellError::Ref),
            Some(Err(error)) => {
                failure = Some(error);
                Expr::Error(CellError::Ref)
            }
            None => {
                failure = Some(TargetGone::Malformed("formula reference replacement ran out".into()));
                Expr::Error(CellError::Ref)
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(format!("={}", gridcraft_formula::print(&resolved)))
}

fn resolve_identity_reference(
    ids: &WorkbookIds,
    workbook: &gridcraft_model::Workbook,
    host_sheet: LiveId,
    identity: &IdentityReference,
) -> Result<Reference, TargetGone> {
    let (sheet, kind) = match identity.sheet {
        IdentitySheetSel::Current { sheet } => {
            if sheet != host_sheet {
                return Err(TargetGone::Malformed("current-sheet formula reference names a different sheet".into()));
            }
            if !identity.span_sheets.is_empty() || !identity.span_references.is_empty() {
                return Err(TargetGone::Malformed("non-span formula reference carries span identities".into()));
            }
            (SheetSel::Current, resolve_identity_ref_kind(ids, sheet, &identity.reference)?)
        }
        IdentitySheetSel::Named { sheet } => {
            if !identity.span_sheets.is_empty() || !identity.span_references.is_empty() {
                return Err(TargetGone::Malformed("non-span formula reference carries span identities".into()));
            }
            let index = ids.resolve_sheet(sheet)?;
            let name = workbook.sheet(index).ok_or(TargetGone::UnknownSheet(sheet))?.name.clone();
            (SheetSel::Named(name), resolve_identity_ref_kind(ids, sheet, &identity.reference)?)
        }
        IdentitySheetSel::Span { start, end } => {
            let start_index = ids.resolve_sheet(start)?;
            let end_index = ids.resolve_sheet(end)?;
            let start_name = workbook.sheet(start_index).ok_or(TargetGone::UnknownSheet(start))?.name.clone();
            let end_name = workbook.sheet(end_index).ok_or(TargetGone::UnknownSheet(end))?.name.clone();
            if identity.span_sheets.is_empty() || identity.span_sheets.len() != identity.span_references.len() {
                return Err(TargetGone::Malformed("3-D formula reference has incomplete span identities".into()));
            }
            let first =
                identity.span_sheets.first().copied().ok_or_else(|| TargetGone::Malformed("3-D formula reference has no span identities".into()))?;
            let last =
                identity.span_sheets.last().copied().ok_or_else(|| TargetGone::Malformed("3-D formula reference has no span identities".into()))?;
            if !((first == start && last == end) || (first == end && last == start)) {
                return Err(TargetGone::Malformed("3-D formula reference endpoints do not match its ordered members".into()));
            }
            let low = start_index.min(end_index);
            let high = start_index.max(end_index);
            let low = u32::try_from(low).map_err(|_| TargetGone::Malformed("3-D formula sheet span is too large".into()))?;
            let high = u32::try_from(high).map_err(|_| TargetGone::Malformed("3-D formula sheet span is too large".into()))?;
            let mut seen = HashSet::with_capacity(identity.span_sheets.len());
            let mut previous = None;
            let mut common = None;
            for (sheet, reference) in identity.span_sheets.iter().copied().zip(&identity.span_references) {
                if !seen.insert(sheet) {
                    return Err(TargetGone::Malformed("3-D formula reference repeats an original sheet identity".into()));
                }
                match ids.sheets.find(sheet) {
                    collab_live::ids::Lookup::At(position) => {
                        if position < low || position > high || previous.is_some_and(|prior| position <= prior) {
                            return Err(TargetGone::Malformed("3-D formula reference does not preserve its original sheet order".into()));
                        }
                        previous = Some(position);
                        let resolved = resolve_identity_ref_kind(ids, sheet, reference)?;
                        if common.as_ref().is_some_and(|current| current != &resolved) {
                            return Err(TargetGone::UnrepresentableReference);
                        }
                        common.get_or_insert(resolved);
                    }
                    collab_live::ids::Lookup::Deleted { .. } => {}
                    collab_live::ids::Lookup::Unknown => return Err(TargetGone::UnknownSheet(sheet)),
                }
            }
            let common = common.ok_or_else(|| TargetGone::Malformed("3-D formula reference has no live original sheet identities".into()))?;
            (SheetSel::Span(start_name, end_name), common)
        }
    };
    Ok(Reference { sheet, kind })
}

fn resolve_identity_ref_kind(ids: &WorkbookIds, target: LiveId, identity: &IdentityRefKind) -> Result<RefKind, TargetGone> {
    ids.resolve_sheet(target)?;
    let axes = ids.by_id.get(&target).ok_or(TargetGone::UnknownSheet(target))?;
    let kind = match *identity {
        IdentityRefKind::Cell { anchor } => RefKind::Cell(resolve_anchor(axes, anchor)?),
        IdentityRefKind::Range { start, end } => RefKind::Range(resolve_anchor(axes, start)?, resolve_anchor(axes, end)?),
        IdentityRefKind::Rows { start, start_abs, end, end_abs } => RefKind::Rows(
            resolve_axis_reference(&axes.rows, start, AxisParam::Rows)?,
            start_abs,
            resolve_axis_reference(&axes.rows, end, AxisParam::Rows)?,
            end_abs,
        ),
        IdentityRefKind::Columns { start, start_abs, end, end_abs } => RefKind::Cols(
            resolve_axis_reference(&axes.cols, start, AxisParam::Columns)?,
            start_abs,
            resolve_axis_reference(&axes.cols, end, AxisParam::Columns)?,
            end_abs,
        ),
    };
    Ok(kind)
}

fn resolve_anchor(axes: &super::SheetIds, anchor: IdentityAnchor) -> Result<Anchor, TargetGone> {
    Ok(Anchor {
        row: resolve_axis_reference(&axes.rows, anchor.row, AxisParam::Rows)?,
        col: resolve_axis_reference(&axes.cols, anchor.col, AxisParam::Columns)?,
        row_abs: anchor.row_abs,
        col_abs: anchor.col_abs,
    })
}

fn resolve_axis_reference(ids: &collab_live::ids::OrderedIds, id: LiveId, axis: AxisParam) -> Result<u32, TargetGone> {
    match (ids.find(id), axis) {
        (collab_live::ids::Lookup::At(position), _) => Ok(position),
        (collab_live::ids::Lookup::Deleted { .. }, AxisParam::Rows) => Err(TargetGone::DeletedRow(id)),
        (collab_live::ids::Lookup::Deleted { .. }, AxisParam::Columns) => Err(TargetGone::DeletedColumn(id)),
        (collab_live::ids::Lookup::Unknown, AxisParam::Rows) => Err(TargetGone::UnknownRow(id)),
        (collab_live::ids::Lookup::Unknown, AxisParam::Columns) => Err(TargetGone::UnknownColumn(id)),
    }
}

fn is_deleted(error: &TargetGone) -> bool {
    matches!(error, TargetGone::DeletedSheet(_) | TargetGone::DeletedRow(_) | TargetGone::DeletedColumn(_) | TargetGone::UnrepresentableReference)
}

fn resolve_position(ids: &WorkbookIds, sheet: LiveId, before: IdentityPosition, axis: AxisParam) -> Result<u32, TargetGone> {
    let axes = ids.by_id.get(&sheet).ok_or(TargetGone::UnknownSheet(sheet))?;
    let ordered = if axis == AxisParam::Rows { &axes.rows } else { &axes.cols };
    match before {
        IdentityPosition::End => Ok(ordered.len()),
        IdentityPosition::Before(id) => resolve_axis_reference(ordered, id, axis),
    }
}

fn resolve_delete_lines(ids: &WorkbookIds, sheet: LiveId, targets: &[LiveId], axis: AxisParam) -> Result<String, TargetGone> {
    let axes = ids.by_id.get(&sheet).ok_or(TargetGone::UnknownSheet(sheet))?;
    let ordered = if axis == AxisParam::Rows { &axes.rows } else { &axes.cols };
    let mut positions = Vec::with_capacity(targets.len());
    for target in targets {
        positions.push(resolve_axis_reference(ordered, *target, axis)?);
    }
    positions.sort_unstable();
    positions.dedup();
    let parts: Result<Vec<_>, _> = positions.into_iter().map(|position| line_text(position, 1, axis)).collect();
    Ok(parts?.join(","))
}

fn line_text(at: u32, count: u32, axis: AxisParam) -> Result<String, TargetGone> {
    let end = at.checked_add(count.saturating_sub(1)).ok_or_else(|| TargetGone::Malformed("line span overflows".into()))?;
    match axis {
        AxisParam::Rows => Ok(format!("{}:{}", u64::from(at) + 1, u64::from(end) + 1)),
        AxisParam::Columns => Ok(format!("{}:{}", gridcraft_core::col_to_letters(at), gridcraft_core::col_to_letters(end))),
    }
}
