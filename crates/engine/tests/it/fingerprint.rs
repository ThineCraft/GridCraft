//! The workbook fingerprint the co-authoring proofs compare: an `entered` digest (what people
//! typed and formatted) and a `computed` digest (what the cells calculate to), with view state
//! left out.

use std::sync::Arc;

use gridcraft_engine::core::{CellError, CellRef, RangeRef, Value};
use gridcraft_engine::fingerprint::{DocId, Fingerprint, fingerprint, first_difference};
use gridcraft_engine::model::{
    AutoFilter, BorderLine, BorderStyle, Cell, Color, DefinedName, FilterCriterion, Formula, HAlign, LineInfo, NumFmt, Sheet, StyleId, Visibility,
    Workbook,
};
use gridcraft_engine::{DocState, Session, Trust};
use serde_json::json;

fn at(a1: &str) -> CellRef {
    CellRef::parse(a1).unwrap()
}

fn range(a1: &str) -> RangeRef {
    RangeRef::parse(a1).unwrap()
}

/// Two sheets with values, text, a formula and a cross-sheet formula.
fn base() -> Workbook {
    let mut wb = Workbook::new();
    wb.sheets.push(Arc::new(Sheet::new("Data")));
    let s = wb.sheet_mut(0).unwrap();
    s.cells.set(at("A1"), Cell::value(Value::Number(1.0)));
    s.cells.set(at("A2"), Cell::value(Value::Number(2.0)));
    s.cells.set(at("A3"), Cell::formula(Formula::new("=SUM(A1:A2)")));
    s.cells.set(at("B2"), Cell::value(Value::from("x")));
    let d = wb.sheet_mut(1).unwrap();
    d.cells.set(at("A1"), Cell::formula(Formula::new("=Sheet1!A3*2")));
    wb
}

/// A session holding `wb`, calculated.
fn session(wb: Workbook) -> Session {
    let mut s = Session::new();
    s.add_document(DocState::new(wb, None, "Book1".into(), Trust::Local));
    s
}

fn fp(wb: Workbook) -> Fingerprint {
    fingerprint(&session(wb), DocId(0))
}

fn styled(wb: &mut Workbook, sheet: usize, a1: &str, f: impl FnOnce(&mut gridcraft_engine::model::Style)) {
    let old = wb.sheet(sheet).unwrap().style_id(at(a1));
    let id = wb.styles.derive(old, f);
    let s = wb.sheet_mut(sheet).unwrap();
    let mut cell = s.cell(at(a1)).cloned().unwrap_or_default();
    cell.style = id;
    s.cells.set(at(a1), cell);
}

#[test]
fn fingerprint_sensitivity() {
    let before = fp(base());
    assert_eq!(before, fp(base()), "the same workbook gives the same fingerprint");

    type Change = fn(&mut Workbook);
    let entered: &[(&str, Change)] = &[
        ("a literal value", |wb| wb.sheet_mut(0).unwrap().cells.set(at("A1"), Cell::value(Value::Number(5.0)))),
        ("text versus number", |wb| wb.sheet_mut(0).unwrap().cells.set(at("A1"), Cell::value(Value::from("1")))),
        ("a formula", |wb| wb.sheet_mut(0).unwrap().cells.set(at("A3"), Cell::formula(Formula::new("=SUM(A1:A1)")))),
        ("a new cell", |wb| wb.sheet_mut(0).unwrap().cells.set(at("Z99"), Cell::value(Value::Bool(true)))),
        ("a cleared cell", |wb| {
            wb.sheet_mut(0).unwrap().cells.remove(at("B2"));
        }),
        ("an error literal", |wb| wb.sheet_mut(0).unwrap().cells.set(at("B2"), Cell::value(Value::Error(CellError::NA)))),
        ("bold", |wb| styled(wb, 0, "B2", |s| s.font.bold = true)),
        ("a font name", |wb| styled(wb, 0, "B2", |s| s.font.name = "Craft Serif".into())),
        ("a font size", |wb| styled(wb, 0, "B2", |s| s.font.size = 14.0)),
        ("a font colour", |wb| styled(wb, 0, "B2", |s| s.font.color = Color::Rgb(0xFF0000))),
        ("a number format", |wb| styled(wb, 0, "A1", |s| s.num_fmt = NumFmt::new("0.00"))),
        ("a fill", |wb| styled(wb, 0, "A1", |s| s.fill = gridcraft_engine::model::Fill::solid(Color::Rgb(0xFFFF00)))),
        ("a border", |wb| styled(wb, 0, "A1", |s| s.borders.bottom = BorderLine { style: BorderStyle::Thick, color: Color::Auto })),
        ("an alignment", |wb| styled(wb, 0, "A1", |s| s.align.h = HAlign::Center)),
        ("wrap text", |wb| styled(wb, 0, "A1", |s| s.align.wrap = true)),
        ("a style on an empty cell", |wb| styled(wb, 0, "D5", |s| s.font.italic = true)),
        ("a style on the second sheet", |wb| styled(wb, 1, "A1", |s| s.font.bold = true)),
        ("a merge", |wb| wb.sheet_mut(0).unwrap().merges.push(range("C1:D1"))),
        ("a hidden row", |wb| {
            wb.sheet_mut(0).unwrap().rows.insert(2, LineInfo { hidden: true, ..LineInfo::default() });
        }),
        ("a hidden column", |wb| {
            wb.sheet_mut(0).unwrap().cols.insert(1, LineInfo { hidden: true, ..LineInfo::default() });
        }),
        ("a row height", |wb| {
            wb.sheet_mut(0).unwrap().rows.insert(3, LineInfo { size: Some(31.0), custom: true, ..LineInfo::default() });
        }),
        ("a column width", |wb| {
            wb.sheet_mut(0).unwrap().cols.insert(2, LineInfo { size: Some(120.0), ..LineInfo::default() });
        }),
        ("a filter", |wb| wb.sheet_mut(0).unwrap().autofilter = Some(AutoFilter { range: range("A1:B3"), criteria: vec![] })),
        ("a defined name", |wb| {
            wb.names.push(DefinedName { name: "Rate".into(), scope: None, formula: "0.07".into(), comment: String::new(), hidden: false })
        }),
        ("a sheet name", |wb| wb.sheet_mut(1).unwrap().name = "Other".into()),
        ("a hidden sheet", |wb| wb.sheet_mut(1).unwrap().visibility = Visibility::Hidden),
        ("the sheet order", |wb| wb.sheets.swap(0, 1)),
    ];
    for (what, change) in entered {
        let mut wb = base();
        change(&mut wb);
        let after = fp(wb);
        assert_ne!(after.entered, before.entered, "{what} changes the entered digest");
    }

    // The filter's criteria alone.
    let filtered = |criteria: Vec<(u32, FilterCriterion)>| {
        let mut wb = base();
        wb.sheet_mut(0).unwrap().autofilter = Some(AutoFilter { range: range("A1:B3"), criteria });
        fp(wb)
    };
    let values = FilterCriterion::Values { values: vec!["1".into()], blanks: false };
    assert_ne!(filtered(vec![]).entered, filtered(vec![(0, values.clone())]).entered, "filter criteria change the entered digest");
    assert_ne!(filtered(vec![(0, values.clone())]).entered, filtered(vec![(1, values)]).entered, "the filtered column changes it");

    // A defined name's formula alone.
    let named = |formula: &str| {
        let mut wb = base();
        wb.names.push(DefinedName { name: "Rate".into(), scope: None, formula: formula.into(), comment: String::new(), hidden: false });
        fp(wb)
    };
    assert_ne!(named("0.07").entered, named("0.08").entered, "a defined name's formula changes the entered digest");

    // Values that calculate differently change the computed digest.
    let mut wb = base();
    wb.sheet_mut(0).unwrap().cells.set(at("A1"), Cell::value(Value::Number(5.0)));
    assert_ne!(fp(wb).computed, before.computed, "a changed input changes the computed digest");

    // A computed value alone (the cached result of a formula, with no recalculation) changes only
    // the computed digest; the bit pattern counts (0 and -0).
    let saved = |a3: Value| {
        let mut wb = base();
        let s = wb.sheet_mut(0).unwrap();
        let mut cell = s.cell(at("A3")).cloned().unwrap();
        cell.value = a3;
        s.cells.set(at("A3"), cell);
        let mut s = Session::new();
        s.add_document(DocState::from_saved(wb, None, "Book1".into(), Trust::Local));
        fingerprint(&s, DocId(0))
    };
    let three = saved(Value::Number(3.0));
    for other in [Value::Number(99.0), Value::Number(0.0), Value::Error(CellError::Div0), Value::from("3"), Value::Bool(true)] {
        let changed = saved(other.clone());
        assert_eq!(changed.entered, three.entered, "{other:?}: a computed value is not entered content");
        assert_ne!(changed.computed, three.computed, "{other:?}: a computed value changes the computed digest");
    }
    assert_ne!(saved(Value::Number(0.0)).computed, saved(Value::Number(-0.0)).computed, "numbers are hashed by their bits");
    assert_ne!(saved(Value::Error(CellError::Div0)).computed, saved(Value::Error(CellError::NA)).computed, "errors by code");

    // View state changes neither digest.
    type View = fn(&mut Workbook);
    let view: &[(&str, View)] = &[
        ("the active sheet", |wb| wb.set_active_sheet(1)),
        ("the saved selection", |wb| wb.sheet_mut(0).unwrap().view_active = at("C7")),
        ("the scroll position", |wb| wb.sheet_mut(0).unwrap().view_top_left = at("K40")),
        ("the zoom", |wb| wb.sheet_mut(0).unwrap().zoom = 150),
        ("frozen panes", |wb| wb.sheet_mut(0).unwrap().freeze = Some((1, 1))),
    ];
    for (what, change) in view {
        let mut wb = base();
        change(&mut wb);
        assert_eq!(fp(wb), before, "{what} is view state");
    }

    // The session's selection, undo history and command journal are not content either.
    let mut s = session(base());
    s.execute("selection.set", json!({"range": "B2:C4"})).unwrap();
    s.execute("cell.set", json!({"cell": "E5", "input": "later undone"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!s.journal.is_empty() && !s.doc().unwrap().redo.is_empty());
    assert_eq!(fingerprint(&s, DocId(0)), before, "selection, undo history and the journal are left out");
}

#[test]
fn fingerprint_is_order_independent() {
    // Two sessions in one process have their own hash-map seeds (the style table's index), and
    // here they also intern the same styles in the opposite order, add the same names and merges
    // in the opposite order, so style ids and list order differ while the content is the same.
    let build = |reverse: bool| {
        let mut wb = base();
        let mut steps: Vec<fn(&mut Workbook)> = vec![
            |wb| styled(wb, 0, "A1", |s| s.font.bold = true),
            |wb| styled(wb, 0, "B2", |s| s.fill = gridcraft_engine::model::Fill::solid(Color::Rgb(0x00FF00))),
            |wb| styled(wb, 1, "C3", |s| s.num_fmt = NumFmt::new("0%")),
            |wb| {
                wb.names.push(DefinedName { name: "Alpha".into(), scope: None, formula: "Sheet1!$A$1".into(), comment: String::new(), hidden: false })
            },
            |wb| wb.names.push(DefinedName { name: "Beta".into(), scope: Some(1), formula: "2".into(), comment: String::new(), hidden: false }),
            |wb| wb.sheet_mut(0).unwrap().merges.push(range("D1:E1")),
            |wb| wb.sheet_mut(0).unwrap().merges.push(range("D3:E4")),
        ];
        if reverse {
            steps.reverse();
        }
        for step in &steps {
            step(&mut wb);
        }
        wb
    };
    let (a, b) = (build(false), build(true));
    let style = |wb: &Workbook, a1: &str| wb.sheet(0).unwrap().style_id(at(a1));
    assert_ne!(style(&a, "A1"), style(&b, "A1"), "the two workbooks really number their styles differently");
    assert_ne!(a.names, b.names, "and list their names in different orders");
    assert_ne!(style(&a, "A1"), StyleId::DEFAULT);
    let (sa, sb) = (session(a), session(b));
    assert_eq!(fingerprint(&sa, DocId(0)), fingerprint(&sb, DocId(0)));
    assert_eq!(first_difference(&sa, &sb, DocId(0)), None);

    // The same commands in two sessions give the same fingerprint.
    let run = || {
        let mut s = Session::new();
        s.new_workbook();
        for (id, params) in [
            ("cell.set", json!({"cell": "A1", "input": "10"})),
            ("cell.set", json!({"cell": "A2", "input": "=A1*3"})),
            ("selection.set", json!({"range": "A1:A2"})),
            ("home.bold", json!({})),
            ("home.mergeCells", json!({})),
            ("home.insertSheet", json!({"name": "Second"})),
            ("cell.set", json!({"cell": "B2", "input": "=Sheet1!A2+1"})),
        ] {
            s.execute(id, params).unwrap();
        }
        s
    };
    let (one, two) = (run(), run());
    assert_eq!(fingerprint(&one, DocId(0)), fingerprint(&two, DocId(0)));
}

#[test]
fn first_difference_names_the_cell() {
    let a = session(base());
    let mut changed = base();
    changed.sheet_mut(0).unwrap().cells.set(at("B2"), Cell::value(Value::from("y")));
    let b = session(changed);
    let message = first_difference(&a, &b, DocId(0)).expect("the workbooks differ");
    assert!(message.contains("Sheet1!B2"), "{message}");
    assert!(message.contains('x') && message.contains('y'), "the message shows both sides: {message}");

    // A difference only in a computed value names that cell too.
    let mut changed = base();
    changed.sheet_mut(0).unwrap().cells.set(at("A2"), Cell::value(Value::Number(7.0)));
    let message = first_difference(&a, &session(changed), DocId(0)).expect("the workbooks differ");
    assert!(message.contains("Sheet1!A2"), "the first difference is the entered value: {message}");

    let mut changed = base();
    styled(&mut changed, 1, "A1", |s| s.font.bold = true);
    let message = first_difference(&a, &session(changed), DocId(0)).expect("the workbooks differ");
    assert!(message.contains("Data!A1"), "{message}");

    // A missing document is a difference, not a panic.
    let empty = Session::new();
    assert!(first_difference(&a, &empty, DocId(0)).is_some());
    assert_eq!(first_difference(&empty, &Session::new(), DocId(0)), None);
}

/// A stored cell with no content, the default style and no row or column style to override (a
/// "blank" cell) changes nothing a person sees, and a save need not keep it. `CellStore::set`
/// never stores one, but one can be left behind by editing a stored cell in place or by a store
/// deserialized as it was written. The fingerprint skips it, so it can't show as a false loss
/// after save and reopen. A blank cell in a styled row or column overrides that style, and counts.
#[test]
fn blank_cells_that_change_nothing_are_skipped() {
    // Stores a bold blank cell, then makes it default-styled in place: `set` would drop it.
    let blank = |wb: &mut Workbook, a1: &str| {
        let bold = bold_style(wb);
        let s = wb.sheet_mut(0).unwrap();
        s.cells.set(at(a1), Cell { value: Value::Empty, formula: None, style: bold, py: None });
        s.cells.get_mut(at(a1)).unwrap().style = StyleId::DEFAULT;
        assert!(s.cells.get(at(a1)).is_some_and(Cell::is_blank), "a blank cell is stored at {a1}");
    };
    let before = fp(base());
    let mut wb = base();
    blank(&mut wb, "Z99");
    assert_eq!(fp(wb), before, "a stored blank cell changes nothing");
    // In a bold column, the default-styled cell is not bold: it counts.
    let mut wb = base();
    let bold = bold_style(&mut wb);
    wb.sheet_mut(0).unwrap().cols.insert(5, LineInfo { style: Some(bold), ..LineInfo::default() });
    let with_column = fp(wb.clone());
    blank(&mut wb, "F7");
    assert_eq!(wb.sheet(0).unwrap().style_id(at("F7")), StyleId::DEFAULT);
    assert_ne!(fp(wb), with_column, "a default cell in a bold column is a format");
    // The same in a bold row.
    let mut wb = base();
    let bold = bold_style(&mut wb);
    wb.sheet_mut(0).unwrap().rows.insert(6, LineInfo { style: Some(bold), ..LineInfo::default() });
    let with_row = fp(wb.clone());
    blank(&mut wb, "F7");
    assert_ne!(fp(wb), with_row, "a default cell in a bold row is a format");
    // A row or column whose style is the default one carries no style, so a blank cell in it is
    // skipped too, as the line itself is.
    let mut wb = base();
    let sheet = wb.sheet_mut(0).unwrap();
    sheet.rows.insert(6, LineInfo { style: Some(StyleId::DEFAULT), ..LineInfo::default() });
    sheet.cols.insert(5, LineInfo { style: Some(StyleId::DEFAULT), ..LineInfo::default() });
    let with_default_lines = fp(wb.clone());
    assert_eq!(with_default_lines, before, "a default-styled row or column changes nothing");
    blank(&mut wb, "F7");
    assert_eq!(fp(wb), with_default_lines, "a blank cell in a default-styled row and column changes nothing");
    // And a formatted blank cell counts.
    let mut wb = base();
    let bold = bold_style(&mut wb);
    wb.sheet_mut(0).unwrap().cells.set(at("Z99"), Cell { value: Value::Empty, formula: None, style: bold, py: None });
    assert_ne!(fp(wb), before, "a bold blank cell is a format");
}

fn bold_style(wb: &mut Workbook) -> StyleId {
    wb.styles.derive(StyleId::DEFAULT, |s| s.font.bold = true)
}

/// A formula's dynamic-array state is fingerprinted as a file writes it: the writer marks a
/// formula as a dynamic array when it was read as one or when its shape needs array evaluation,
/// and the reader then reads it as one. So a formula the shape analysis marks anyway has the same
/// fingerprint whether or not its flag is set (as after a save and reopen), while the flag on a
/// formula the analysis would not mark is content.
#[test]
fn dynamic_array_state_is_fingerprinted_as_written() {
    let with = |text: &str, dynamic: bool| {
        let mut wb = base();
        let mut f = Formula::new(text);
        f.dynamic = dynamic;
        wb.sheet_mut(0).unwrap().cells.set(at("C1"), Cell::formula(f));
        fp(wb)
    };
    // A lookup over deleted ranges: its shape is unknown, so the writer marks it.
    let lookup = "=XLOOKUP(1,#REF!,#REF!,\"none\")";
    assert_eq!(with(lookup, false), with(lookup, true));
    let lifted = "=A1:A2*2";
    assert_eq!(with(lifted, false), with(lifted, true));
    // A scalar formula read with dynamic-array metadata keeps it: that is content.
    let scalar = "=SUM(A1:A2)";
    assert_ne!(with(scalar, false), with(scalar, true));
}
