//! Workbook fingerprints for the co-authoring proofs: two SHA-256 digests over a canonical walk
//! of a workbook, so two engines (two sessions, native and WebAssembly, before and after a save)
//! can be compared cheaply and exactly.
//!
//! - `entered` covers what people typed and formatted: each sheet's name and hidden state; each
//!   stored cell's position (a blank one that no row or column style applies to is skipped),
//!   formula (as the formula printer prints it) or literal value, and its style resolved to
//!   values (number format, font, fill, borders, alignment, protection), never a style id; row
//!   heights and column widths that differ from the default, hidden rows and
//!   columns (except rows an applied filter hides, see `computed`) and whole-row or whole-column
//!   styles; merges, sorted; the filter range and its
//!   criteria; and the defined names, sorted.
//! - `computed` covers what the cells calculate to: each cell's value, each spilled value, and
//!   the rows an applied filter hides (they follow from the values, often calculated ones).
//!   Numbers are written as their IEEE-754 bits and errors by code.
//!
//! View state is left out: the active sheet, the selection, the scroll position, the zoom,
//! frozen panes, gridlines and the view mode, and the session's undo history and command journal.
//!
//! The walk is canonical: sheets in order, cells in row-major order, and every list whose order
//! is not content (merges, names, filter criteria) sorted first. It never iterates a hash map.
//! Each item is a key (where it is, `Sheet1!B2`) and its canonical text; the digests hash both,
//! length-prefixed, so [`first_difference`] can name the first item that differs.

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, Sheet, Style, Workbook};
use sha2::{Digest, Sha256};

use crate::Session;

/// A workbook open in a session: its index in [`Session::documents`]. Two sessions number their
/// documents the same way when they open them in the same order, unlike document uids, which are
/// unique across the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocId(pub usize);

/// The two digests of a workbook (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    pub entered: [u8; 32],
    pub computed: [u8; 32],
}

/// One item in the canonical walk used to build a [`Fingerprint`].
///
/// Cross-platform tests persist these items beside the native digests so a mismatch can name the
/// first visible workbook property or computed cell that differs, rather than only two hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintItem {
    pub stream: &'static str,
    pub key: String,
    pub canonical: String,
}

impl Fingerprint {
    /// Both digests as lowercase hex, `entered` first, for messages and reports.
    pub fn hex(&self) -> (String, String) {
        let hex = |d: &[u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        (hex(&self.entered), hex(&self.computed))
    }
}

/// Which digest an item goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Entered,
    Computed,
}

impl Stream {
    fn label(self) -> &'static str {
        match self {
            Stream::Entered => "entered",
            Stream::Computed => "computed",
        }
    }
}

/// The fingerprint of document `doc` in `session`. A document that isn't open has the
/// fingerprint of a single "missing" item, which no workbook has.
pub fn fingerprint(session: &Session, doc: DocId) -> Fingerprint {
    match session.documents().get(doc.0) {
        Some(d) => workbook_fingerprint(&d.wb),
        None => digest(|out| missing(out)),
    }
}

/// The canonical items hashed by [`fingerprint`], in their digest order.
pub fn fingerprint_items(session: &Session, doc: DocId) -> Vec<FingerprintItem> {
    let items = match session.documents().get(doc.0) {
        Some(d) => collect(|out| walk(&d.wb, out)),
        None => collect(|out| missing(out)),
    };
    items.into_iter().map(|(stream, key, canonical)| FingerprintItem { stream: stream.label(), key, canonical }).collect()
}

/// The fingerprint of a workbook.
pub fn workbook_fingerprint(wb: &Workbook) -> Fingerprint {
    digest(|out| walk(wb, out))
}

/// The first item in which document `doc` differs between the two sessions, described for a
/// test message (`entered Sheet1!B2: text "x" … ≠ text "y" …`); `None` when the fingerprints'
/// walks are the same.
pub fn first_difference(a: &Session, b: &Session, doc: DocId) -> Option<String> {
    let items = |s: &Session| match s.documents().get(doc.0) {
        Some(d) => collect(|out| walk(&d.wb, out)),
        None => collect(|out| missing(out)),
    };
    difference(&items(a), &items(b))
}

/// [`first_difference`] for two workbooks.
pub fn workbook_first_difference(a: &Workbook, b: &Workbook) -> Option<String> {
    difference(&collect(|out| walk(a, out)), &collect(|out| walk(b, out)))
}

type Items = Vec<(Stream, String, String)>;

fn collect(run: impl FnOnce(&mut dyn FnMut(Stream, &str, &str))) -> Items {
    let mut items = Items::new();
    run(&mut |stream, key, canon| items.push((stream, key.to_string(), canon.to_string())));
    items
}

fn difference(a: &Items, b: &Items) -> Option<String> {
    for stream in [Stream::Entered, Stream::Computed] {
        let mut left = a.iter().filter(|i| i.0 == stream);
        let mut right = b.iter().filter(|i| i.0 == stream);
        loop {
            match (left.next(), right.next()) {
                (None, None) => break,
                (Some((_, k, c)), None) => return Some(format!("{}: {k} ({c}) only in the first", stream.label())),
                (None, Some((_, k, c))) => return Some(format!("{}: {k} ({c}) only in the second", stream.label())),
                (Some((_, ka, ca)), Some((_, kb, cb))) => {
                    if ka != kb {
                        return Some(format!("{}: {ka} ({ca}) in the first where the second has {kb} ({cb})", stream.label()));
                    }
                    if ca != cb {
                        return Some(format!("{} {ka}: {ca} ≠ {cb}", stream.label()));
                    }
                }
            }
        }
    }
    None
}

fn digest(run: impl FnOnce(&mut dyn FnMut(Stream, &str, &str))) -> Fingerprint {
    let mut entered = Sha256::new();
    let mut computed = Sha256::new();
    run(&mut |stream, key, canon| {
        let h = match stream {
            Stream::Entered => &mut entered,
            Stream::Computed => &mut computed,
        };
        for part in [key, canon] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
    });
    let bytes = |h: Sha256| {
        let mut out = [0u8; 32];
        for (o, b) in out.iter_mut().zip(h.finalize().iter()) {
            *o = *b;
        }
        out
    };
    Fingerprint { entered: bytes(entered), computed: bytes(computed) }
}

fn missing(out: &mut dyn FnMut(Stream, &str, &str)) {
    out(Stream::Entered, "document", "missing");
    out(Stream::Computed, "document", "missing");
}

/// A string in JSON quoting: unambiguous, and the same on every platform.
fn quote(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn style_text(style: &Style) -> String {
    serde_json::to_string(style).unwrap_or_else(|e| format!("unserializable style: {e}"))
}

fn number(n: f64) -> String {
    format!("num {:016x} ({n})", n.to_bits())
}

/// A value: numbers by their bits, errors by code, text quoted.
fn value_text(v: &Value) -> String {
    match v {
        Value::Empty => "empty".into(),
        Value::Number(n) => number(*n),
        Value::Text(t) => format!("text {}", quote(t)),
        Value::Bool(b) => format!("bool {b}"),
        Value::Error(e) => format!("error {} {}", e.code(), e.as_str()),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(value_text).collect();
            format!("array {}x{} [{}]", a.rows, a.cols, items.join("; "))
        }
    }
}

/// A cell's entered content: Python code, a formula as the printer prints it, or the literal.
/// A formula is marked `dynamic` as a file writes it: when it was read as a dynamic array, when
/// it anchors a spill, or when its shape needs array evaluation (the writer marks all three,
/// and the reader reads them back as dynamic arrays).
fn content_text(wb: &Workbook, si: usize, sheet: &Sheet, c: CellRef, cell: &Cell) -> String {
    if let Some(py) = &cell.py {
        return format!("python {}", quote(&py.code));
    }
    if let Some(f) = &cell.formula {
        let printed = f.with_expr(gridcraft_formula::print).unwrap_or_else(|| f.text.clone());
        let mut s = format!("formula {}", quote(&printed));
        if let Some(r) = f.array {
            s.push_str(&format!(" array {}", r.a1()));
        }
        let written_dynamic = f.array.is_none()
            && (f.dynamic || sheet.spill_ranges.get(&c).is_some_and(|r| !r.is_single()) || gridcraft_xlsx::needs_array(wb, si, c, f));
        if written_dynamic {
            s.push_str(" dynamic");
        }
        return s;
    }
    value_text(&cell.value)
}

fn range_key(r: &RangeRef) -> (u32, u32, u32, u32) {
    (r.start.row, r.start.col, r.end.row, r.end.col)
}

fn cell_key(sheet: &Sheet, c: CellRef) -> String {
    format!("{}!{}", sheet.name, c.a1())
}

fn walk(wb: &Workbook, out: &mut dyn FnMut(Stream, &str, &str)) {
    for (i, sheet) in wb.sheets.iter().enumerate() {
        let sheet_key = format!("sheet {}", i + 1);
        out(
            Stream::Entered,
            &sheet_key,
            &format!(
                "name {} visibility {:?} default width {:08x} default height {:08x}",
                quote(&sheet.name),
                sheet.visibility,
                sheet.default_col_width.to_bits(),
                sheet.default_row_height.to_bits()
            ),
        );
        entered_sheet(wb, i, sheet, out);
        out(Stream::Computed, &sheet_key, &format!("name {}", quote(&sheet.name)));
        computed_sheet(sheet, out);
    }
    let mut names: Vec<_> = wb.names.iter().collect();
    names.sort_by(|a, b| {
        (a.scope, a.name.to_lowercase(), &a.name, &a.formula, &a.comment, a.hidden).cmp(&(
            b.scope,
            b.name.to_lowercase(),
            &b.name,
            &b.formula,
            &b.comment,
            b.hidden,
        ))
    });
    for n in names {
        let scope = match n.scope {
            None => "workbook".to_string(),
            Some(s) => format!("sheet {}", s + 1),
        };
        let formula = gridcraft_formula::parse(&n.formula).map(|e| gridcraft_formula::print(&e)).unwrap_or_else(|_| n.formula.clone());
        out(
            Stream::Entered,
            &format!("name {} ({scope})", n.name),
            &format!("formula {} comment {} hidden {}", quote(&formula), quote(&n.comment), n.hidden),
        );
    }
}

fn entered_sheet(wb: &Workbook, si: usize, sheet: &Sheet, out: &mut dyn FnMut(Stream, &str, &str)) {
    for (c, cell) in sheet.cells.iter() {
        // A blank cell (no content, the default style) where no row or column style applies
        // changes nothing a person sees, and a save need not keep it. The store never keeps one
        // through `set`, but an in-place edit or a deserialized store can. A line style of
        // `Some(DEFAULT)` is no style, as in the line loop below and the file's column writer.
        let line_style = |info: Option<&gridcraft_model::LineInfo>| info.and_then(|l| l.style).filter(|s| *s != gridcraft_model::StyleId::DEFAULT);
        if cell.is_blank() && line_style(sheet.rows.get(&c.row)).is_none() && line_style(sheet.cols.get(&c.col)).is_none() {
            continue;
        }
        let style = style_text(wb.styles.get(cell.style));
        out(Stream::Entered, &cell_key(sheet, c), &format!("{} style {style}", content_text(wb, si, sheet, c, cell)));
    }
    // Rows an applied filter hides follow from the cells' values (often calculated ones), not
    // from what anyone entered: they are in the computed stream (`computed_sheet`).
    let filter_spans = sheet.filter_row_spans();
    for (what, lines, default) in [("row", &sheet.rows, sheet.default_row_height), ("column", &sheet.cols, sheet.default_col_width)] {
        for (index, info) in lines.iter() {
            let size = info.size.filter(|s| s.to_bits() != default.to_bits());
            let style = info.style.filter(|s| *s != gridcraft_model::StyleId::DEFAULT);
            let hidden = info.hidden && !(what == "row" && in_spans(&filter_spans, *index));
            if size.is_none() && !hidden && style.is_none() {
                continue;
            }
            let size = size.map_or_else(|| "default".to_string(), |s| format!("{:08x} ({s})", s.to_bits()));
            let style = style.map_or_else(|| "none".to_string(), |s| style_text(wb.styles.get(s)));
            out(Stream::Entered, &format!("{}!{what} {}", sheet.name, u64::from(*index) + 1), &format!("size {size} hidden {hidden} style {style}"));
        }
    }
    let mut merges: Vec<&RangeRef> = sheet.merges.iter().collect();
    merges.sort_by_key(|r| range_key(r));
    for m in merges {
        out(Stream::Entered, &format!("{}!merge", sheet.name), &m.a1());
    }
    if let Some(filter) = &sheet.autofilter {
        let mut criteria: Vec<String> = filter
            .criteria
            .iter()
            .map(|(col, c)| format!("{col}: {}", serde_json::to_string(c).unwrap_or_else(|e| format!("unserializable criterion: {e}"))))
            .collect();
        // Sorted by column (the text starts with it, then the criterion breaks ties).
        criteria.sort_by_key(|s| (s.split(':').next().and_then(|n| n.parse::<u32>().ok()), s.clone()));
        out(Stream::Entered, &format!("{}!filter", sheet.name), &format!("range {} criteria [{}]", filter.range.a1(), criteria.join("; ")));
    }
}

fn computed_sheet(sheet: &Sheet, out: &mut dyn FnMut(Stream, &str, &str)) {
    for (c, cell) in sheet.cells.iter() {
        if cell.has_content() {
            out(Stream::Computed, &cell_key(sheet, c), &value_text(&cell.value));
        }
    }
    for (c, v) in sheet.spill.iter() {
        out(Stream::Computed, &format!("{} spilled", cell_key(sheet, *c)), &value_text(v));
    }
    let filter_spans = sheet.filter_row_spans();
    for (index, info) in sheet.rows.iter() {
        if info.hidden && in_spans(&filter_spans, *index) {
            out(Stream::Computed, &format!("{}!row {}", sheet.name, u64::from(*index) + 1), "filtered out");
        }
    }
}

fn in_spans(spans: &[(u32, u32)], row: u32) -> bool {
    spans.iter().any(|&(first, last)| row >= first && row <= last)
}
