use std::collections::HashSet;
use std::fmt;

use collab_live::ids::LiveId;
use protocol::live::MAX_PAYLOAD_BYTES;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::CoauthorError;

/// One command in the `gridcraft/1` payload format.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "id")]
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
    NumberFormat {
        sheet: LiveId,
        range: Json,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<NumberFormatName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    #[serde(rename = "home.fontName")]
    FontName { sheet: LiveId, range: Json, name: String },
    #[serde(rename = "home.fontSize")]
    FontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.fontColor")]
    FontColor { sheet: LiveId, range: Json, color: CommandColor },
    #[serde(rename = "home.fillColor")]
    FillColor { sheet: LiveId, range: Json, color: CommandColor },
    #[serde(rename = "home.borders")]
    Borders {
        sheet: LiveId,
        range: Json,
        preset: BorderPreset,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<CommandBorderStyle>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<CommandColor>,
    },
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
    Underline { sheet: LiveId, range: Json, on: bool, style: UnderlineStyle },
    #[serde(rename = "home.doubleUnderline")]
    DoubleUnderline { sheet: LiveId, range: Json, on: bool, style: UnderlineStyle },
    #[serde(rename = "home.increaseFontSize")]
    IncreaseFontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.decreaseFontSize")]
    DecreaseFontSize { sheet: LiveId, range: Json, size: f64 },
    #[serde(rename = "home.formatCells")]
    FormatCells { sheet: LiveId, range: Json, style: StylePatch },
    #[serde(rename = "data.filterSet")]
    FilterSet { sheet: LiveId, range: Json },
    #[serde(rename = "data.filterRemove")]
    FilterRemove { sheet: LiveId, range: Json },
    #[serde(rename = "data.filterBy")]
    FilterBy {
        sheet: LiveId,
        range: Json,
        column: LiveId,
        #[serde(flatten)]
        criteria: FilterCriteria,
    },
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
    pub tint: f64,
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
    pub style: StyleBorderStyle,
    pub color: StyleColor,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilterValue {
    String(String),
    Number(f64),
    Bool(bool),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterCustom {
    pub custom: CustomFilter,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomFilter {
    pub op: String,
    pub value: FilterValue,
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
    if !is_shared_command(id) {
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
        "cell.set" => &["id", "sheet", "cell", "input", "originYear", "references"],
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

fn missing_required_field(id: &str, command: &serde_json::Map<String, Json>) -> Option<&'static str> {
    let fields: &[&str] = match id {
        "cell.set" => &["sheet", "cell", "input", "originYear", "references"],
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
