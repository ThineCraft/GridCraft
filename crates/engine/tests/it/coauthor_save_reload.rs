//! Save and reopen (co-authoring plan G1, Task 5): a checkpoint written to .xlsx and read back
//! is the same workbook, so a co-author who loads it continues from the agreed state.
//!
//! Every case is a workbook made by a sequence of commands: the checked-in replay fixtures
//! (`tests/fixtures/coauthor/`), generated sequences, and hand-made edge cases (sheet names that
//! need quoting, renamed and deleted sheets behind `#REF!`, extreme numbers and negative zero,
//! empty text against empty cells, awkward text, merged areas, a filter, defined names). Each is
//! saved with `file.saveBytes` and read back with `io::open_bytes`, the engine's save and open
//! paths.
//!
//! - `save_reload_lossless`: the reopened workbook, with the values its file holds
//!   (`DocState::from_saved`, as a checkpoint is loaded), has the same `entered` and `computed`
//!   digests. Opened as `file.open` opens a file (recalculating), and then both it and the
//!   original recalculated in full under the sequence's clock and RAND seed, the `computed`
//!   digests agree too, so the formulas, not only their cached values, survive.
//! - `save_is_deterministic`: two engines that applied the same commands write identical bytes.
//! - `save_reload_save_is_stable`: writing, reopening and writing again gives identical bytes.
//!
//! - `known_loss_reopen_refits_rows_from_values` (ignored): the one loss left for G4, row heights
//!   that opening a file re-fits.
//!
//! Knob: `GRIDCRAFT_COAUTHOR_SAVE_SEEDS` (default 200) generated sequences of 60 steps, seeds
//! 2000 onwards.

use std::path::PathBuf;

use gridcraft_engine::core::{CellError, CellRef, Value};
use gridcraft_engine::fingerprint::{DocId, fingerprint, workbook_fingerprint, workbook_first_difference};
use gridcraft_engine::model::Workbook;
use gridcraft_engine::{DocState, Session, Trust, io};
use serde_json::json;

use crate::sequences::{DEFAULT_NOW, Sequence, Step, apply, fresh_session, generate};

/// The file name the engine saves a new workbook under (`file.saveBytes` on "Book1").
const NAME: &str = "Book1.xlsx";

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("coauthor")
}

// ---------------------------------------------------------------- cases

struct Case {
    name: String,
    seq: Sequence,
}

fn set(sheet: &str, cell: &str, input: &str) -> Step {
    Step::new("cell.set", json!({"sheet": sheet, "cell": cell, "input": input}))
}

fn step(id: &str, params: serde_json::Value) -> Step {
    Step::new(id, params)
}

fn case(name: &str, seed: u64, steps: Vec<Step>) -> Case {
    Case { name: name.to_string(), seq: Sequence { seed, now: DEFAULT_NOW, steps } }
}

/// The edge cases of the plan's Review focus 4. Steps other than the shared commands (merges,
/// filters, names, sheet activation) only build the workbook; they are not replayed as shared.
fn edge_cases() -> Vec<Case> {
    let quoted = ["Q 1 Plan", "O'Brien", "2024", "A1", "R1C1", "Données-€", "TRUE", "Sheet (2)"];
    let mut quoting = Vec::new();
    for (i, name) in quoted.iter().enumerate() {
        quoting.push(step("home.insertSheet", json!({"name": name, "before": i + 2})));
        quoting.push(set(name, "A1", &(i + 1).to_string()));
        quoting.push(set(name, "B2", &format!("{}", (i + 1) * 10)));
    }
    quoting.extend([
        set("Sheet1", "A1", "='Q 1 Plan'!A1+'O''Brien'!A1+'2024'!A1+'A1'!A1"),
        set("Sheet1", "A2", "='R1C1'!A1*'Données-€'!B2-'TRUE'!A1+'Sheet (2)'!B2"),
        set("Sheet1", "A3", "=SUM('Q 1 Plan'!A1:B2,'O''Brien'!A1:B2)"),
        set("Sheet1", "A4", "=VLOOKUP(1,'A1'!A1:B2,1,FALSE)"),
        set("Sheet1", "A5", "=COUNTIF('2024'!A1:B2,\">5\")"),
        set("O'Brien", "C1", "=Sheet1!A1+'Q 1 Plan'!B2"),
    ]);

    let renamed = vec![
        step("home.insertSheet", json!({"name": "Old", "before": 2})),
        step("home.insertSheet", json!({"name": "Gone", "before": 3})),
        set("Old", "A1", "5"),
        set("Gone", "A1", "7"),
        set("Gone", "B1", "8"),
        set("Sheet1", "A1", "=Old!A1*2"),
        set("Sheet1", "A2", "=Gone!A1+1"),
        set("Sheet1", "A3", "=SUM(Gone!A1:B1)+Old!A1"),
        set("Sheet1", "A4", "=IFERROR(Gone!A1,\"gone\")"),
        set("Old", "B1", "=Gone!A1&\"!\""),
        step("sheet.rename", json!({"sheet": "Old", "name": "New name's"})),
        step("home.deleteSheet", json!({"sheet": "Gone"})),
        set("Sheet1", "A5", "='New name''s'!A1+1"),
        // A formula that names a sheet that never existed, typed with an error literal.
        set("Sheet1", "A6", "=#REF!+1"),
        // Rows and columns deleted under references.
        set("Sheet1", "C1", "1"),
        set("Sheet1", "D1", "=C1*3"),
        step("home.deleteColumns", json!({"sheet": "Sheet1", "cols": "C:C"})),
    ];

    let numbers: Vec<Step> = [
        ("A1", "1E+308"),
        ("A2", "-1E+308"),
        ("A3", "1.7976931348623157E+308"),
        ("A4", "2.2250738585072014E-308"),
        ("A5", "4.9E-324"),
        ("A6", "1E-300"),
        ("A7", "-0"),
        ("A8", "0.30000000000000004"),
        ("A9", "123456789012345678"),
        ("A10", "9007199254740993"),
        ("A11", "-1.5E-10"),
        ("A12", "3.141592653589793"),
        ("A13", "0.1"),
        ("A14", "1E+15"),
        ("A15", "123456789.123456789"),
        ("B1", "=0.1+0.2"),
        ("B2", "=2^-1074"),
        ("B3", "=-0"),
        ("B4", "=A7*1"),
        ("B5", "=1E+308*10"),
        ("B6", "=1/3"),
        ("B7", "=A1/A6"),
        ("B8", "=-A13"),
        ("B9", "=A5/2"),
        ("B10", "=1E+307*-17"),
        ("B11", "=2/3*1E-305"),
        ("B12", "=SQRT(2)"),
        ("B13", "=PRODUCT(A1,A2)"),
        // Negative zero: typed `-0` is stored as 0, but arithmetic can still make -0.
        ("B14", "=A7*-1"),
        ("B15", "=ROUND(-0.0001,0)"),
        ("B16", "=-1E-200*1E-200"),
        ("B17", "=B14"),
        ("B18", "=1/B15"),
    ]
    .iter()
    .map(|(c, i)| set("Sheet1", c, i))
    .collect();

    let long = "x".repeat(5_000);
    let texts = vec![
        set("Sheet1", "A1", "=\"\""),
        set("Sheet1", "A2", "'"),
        set("Sheet1", "A3", " "),
        set("Sheet1", "A4", "=A1&A2"),
        set("Sheet1", "A5", "=ISBLANK(A2)"),
        set("Sheet1", "A6", "=ISBLANK(A9)"),
        set("Sheet1", "A7", "=LEN(A1)+LEN(A2)"),
        set("Sheet1", "A8", "=A9"),
        set("Sheet1", "A10", "=A1=A9"),
        set("Sheet1", "A11", "=COUNTA(A1:A3,A9)"),
        set("Sheet1", "A12", "=IF(A9=\"\",\"\",1)"),
        set("Sheet1", "B1", "'00123"),
        set("Sheet1", "B2", "'=not a formula"),
        set("Sheet1", "B3", "line one\nline two"),
        set("Sheet1", "B4", "Grüße 🌍 中文 עברית"),
        set("Sheet1", "B5", "_x0041_ stays as typed"),
        set("Sheet1", "B6", "tab\there"),
        set("Sheet1", "B7", "<tag attr=\"v\">&amp;</tag>"),
        set("Sheet1", "B8", "  leading and trailing  "),
        set("Sheet1", "B9", &long),
        set("Sheet1", "B10", "TRUE"),
        set("Sheet1", "B11", "#N/A"),
        set("Sheet1", "B12", "'TRUE"),
        set("Sheet1", "B13", "=\"say \"\"hi\"\"\""),
        set("Sheet1", "B14", "=B9&\"\""),
        set("Sheet1", "B15", "'#DIV/0!"),
        set("Sheet1", "B16", "=1/0"),
        set("Sheet1", "B17", "=NA()"),
        set("Sheet1", "B18", "control \u{1} char"),
        // Formats on cells that hold nothing, and formats left after the content went.
        step("home.bold", json!({"sheet": "Sheet1", "range": "C1:C3", "on": true})),
        set("Sheet1", "D1", "gone"),
        step("home.fillColor", json!({"sheet": "Sheet1", "range": "D1", "color": "#FFFF00"})),
        step("edit.clearContents", json!({"sheet": "Sheet1", "range": "D1"})),
        set("Sheet1", "D2", "gone too"),
        step("edit.clearContents", json!({"sheet": "Sheet1", "range": "D2"})),
    ];

    let merges = vec![
        set("Sheet1", "A1", "Title"),
        set("Sheet1", "C3", "inside"),
        set("Sheet1", "E5", "=A1&\"!\""),
        step("home.mergeCenter", json!({"sheet": "Sheet1", "range": "A1:D1"})),
        step("home.mergeCells", json!({"sheet": "Sheet1", "range": "B3:C6"})),
        step("home.mergeAcross", json!({"sheet": "Sheet1", "range": "F2:H4"})),
        step("home.insertSheet", json!({"name": "Two", "before": 2})),
        step("home.mergeCells", json!({"sheet": "Two", "range": "A1:A3"})),
        step("home.borders", json!({"sheet": "Sheet1", "range": "A1:D1", "preset": "all"})),
        step("home.insertRows", json!({"sheet": "Sheet1", "rows": "2:2"})),
        step("home.insertColumns", json!({"sheet": "Sheet1", "cols": "A:A"})),
    ];

    let filter_data: [[&str; 3]; 6] = [["Name", "Kind", "Qty"], ["a", "x", "1"], ["b", "y", "2"], ["c", "x", "30"], ["d", "", "4"], ["e", "z", "50"]];
    let mut filter = vec![step("home.insertSheet", json!({"name": "Stock list", "before": 2}))];
    for (r, row) in filter_data.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                let cell = format!("{}{}", ["A", "B", "C"][c], r + 1);
                filter.push(set("Sheet1", &cell, v));
                filter.push(set("Stock list", &cell, v));
            }
        }
    }
    filter.extend([
        set("Sheet1", "E1", "=SUBTOTAL(9,C2:C6)"),
        step("sheet.activate", json!({"sheet": "Sheet1"})),
        step("data.filter", json!({"range": "A1:C6"})),
        step("data.filterBy", json!({"column": "B", "values": ["x"], "blanks": true})),
        step("data.filterBy", json!({"column": "C", "custom": {"op": ">=", "value": 2, "op2": "<", "value2": 40, "and": true}})),
        step("sheet.activate", json!({"sheet": "Stock list"})),
        step("data.filter", json!({"range": "A1:C6"})),
        step("data.filterBy", json!({"column": "A", "values": ["a", "e"]})),
        step("sheet.activate", json!({"sheet": "Sheet1"})),
    ]);

    let names = vec![
        step("home.insertSheet", json!({"name": "Rates", "before": 2})),
        set("Rates", "A1", "0.2"),
        set("Rates", "A2", "0.3"),
        step("formulas.defineName", json!({"name": "Rate", "refersTo": "=Rates!$A$1"})),
        step("formulas.defineName", json!({"name": "Both", "refersTo": "=Rates!$A$1:$A$2", "comment": "two rates"})),
        step("formulas.defineName", json!({"name": "Local", "refersTo": "=Sheet1!$B$1", "scope": "Sheet1"})),
        step("formulas.defineName", json!({"name": "Twice", "refersTo": "=Rate*2"})),
        set("Sheet1", "B1", "100"),
        set("Sheet1", "A1", "=B1*Rate"),
        set("Sheet1", "A2", "=SUM(Both)+Twice+Local"),
        step("sheet.rename", json!({"sheet": "Rates", "name": "Rate table"})),
    ];

    let styles = vec![
        set("Sheet1", "A1", "1234.5"),
        step("home.numberFormat", json!({"sheet": "Sheet1", "range": "A1:A3", "format": "Currency"})),
        step("home.numberFormat", json!({"sheet": "Sheet1", "range": "B1", "format": "Percentage"})),
        step("home.fontName", json!({"sheet": "Sheet1", "range": "A1:B2", "name": "Arial"})),
        step("home.fontSize", json!({"sheet": "Sheet1", "range": "A1", "size": 22})),
        step("home.fontColor", json!({"sheet": "Sheet1", "range": "A2", "color": "#C00000"})),
        step("home.italic", json!({"sheet": "Sheet1", "range": "A3", "on": true})),
        step("home.borders", json!({"sheet": "Sheet1", "range": "C3:D4", "preset": "outside"})),
        step("home.alignRight", json!({"sheet": "Sheet1", "range": "A1:A3", "on": true})),
        step("home.alignTop", json!({"sheet": "Sheet1", "range": "B1:B3"})),
        step("home.alignMiddle", json!({"sheet": "Sheet1", "range": "C1"})),
        step("home.wrapText", json!({"sheet": "Sheet1", "range": "D1", "on": true})),
        set("Sheet1", "D1", "a long line of text that wraps in a narrow column"),
        step("home.deleteRows", json!({"sheet": "Sheet1", "rows": "2:2"})),
        step("home.insertColumns", json!({"sheet": "Sheet1", "cols": "B:C"})),
    ];

    vec![
        case("edge-quoted-sheet-names", 501, quoting),
        case("edge-renamed-and-deleted-sheets", 502, renamed),
        case("edge-numbers", 503, numbers),
        case("edge-empty-text-and-awkward-text", 504, texts),
        case("edge-merged-areas", 505, merges),
        case("edge-filters", 506, filter),
        case("edge-defined-names", 507, names),
        case("edge-styles", 508, styles),
    ]
}

fn fixture_cases() -> Vec<Case> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json") && p.file_name().is_none_or(|x| x != "native-digests.json"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    assert!(paths.len() >= 26, "the coauthor fixtures are missing ({} found)", paths.len());
    paths
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let seq = Sequence::from_text(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            Case { name, seq }
        })
        .collect()
}

fn generated_cases() -> Vec<Case> {
    let n = env_num("GRIDCRAFT_COAUTHOR_SAVE_SEEDS", 200);
    (2_000..2_000 + n).map(|seed| Case { name: format!("seed {seed}"), seq: generate(seed, 60) }).collect()
}

fn all_cases() -> Vec<Case> {
    let mut out = edge_cases();
    out.extend(fixture_cases());
    out.extend(generated_cases());
    out
}

// ---------------------------------------------------------------- save and open

/// Applies the case's steps to a fresh session. Edge cases must apply cleanly (they would
/// prove nothing otherwise); generated steps may fail, as in the replay.
fn build(c: &Case) -> Result<Session, String> {
    let mut s = fresh_session(&c.seq);
    let strict = c.name.starts_with("edge-");
    for (i, st) in c.seq.steps.iter().enumerate() {
        if let Err(e) = apply(&mut s, st)
            && strict
        {
            return Err(format!("{}: step {i} `{}` {} failed: {e}", c.name, st.id, st.params));
        }
    }
    Ok(s)
}

/// The workbook's bytes as the engine saves it (`file.saveBytes`).
fn save(s: &mut Session) -> Result<Vec<u8>, String> {
    let r = s.execute("file.saveBytes", json!({"format": "xlsx"})).map_err(|e| format!("save failed: {e}"))?;
    let b64 = r["base64"].as_str().ok_or("save gave no bytes")?;
    io::base64_decode(b64).ok_or_else(|| "save gave bad base64".to_string())
}

/// The bytes read back as a checkpoint is loaded: the values the file holds, with the
/// sequence's clock and RAND seed for later edits.
fn reopen(bytes: &[u8], seq: &Sequence) -> Result<Session, String> {
    let (wb, _warnings) = io::open_bytes(NAME, bytes).map_err(|e| format!("reopen failed: {e}"))?;
    let mut d = DocState::from_saved(wb, None, "Book1".into(), Trust::Local);
    d.calc.fixed_now = Some(seq.now);
    d.calc.set_rand_seed(seq.seed);
    let mut s = Session::new();
    s.add_document(d);
    Ok(s)
}

/// Recalculates the active document from scratch under the sequence's clock and a RAND stream
/// restarted from its seed.
fn recalc_fully(s: &mut Session, seq: &Sequence) -> Result<(), String> {
    let d = s.doc_mut().map_err(|e| e.to_string())?;
    d.calc.fixed_now = Some(seq.now);
    d.calc.set_rand_seed(seq.seed);
    s.execute("formulas.calculateNow", json!({})).map_err(|e| format!("calculateNow: {e}"))?;
    Ok(())
}

fn first_diff(a: &Session, b: &Session) -> String {
    match (a.doc(), b.doc()) {
        (Ok(x), Ok(y)) => workbook_first_difference(&x.wb, &y.wb).unwrap_or_else(|| "(digests differ but no item does)".into()),
        _ => "(a document is missing)".into(),
    }
}

/// The workbook as its file caches it: the writer stores a circular reference's `#CIRC!` as 0
/// and `#SPILL!`, `#CALC!` and the Python errors as `#VALUE!`, as Excel does, because the file
/// format has no codes for them. Every other value is cached as it is.
fn as_cached(wb: &Workbook) -> Workbook {
    let mut wb = wb.clone();
    for i in 0..wb.sheets.len() {
        let Some(sh) = wb.sheet_mut(i) else { continue };
        let changed: Vec<(CellRef, Value)> = sh
            .cells
            .iter()
            .filter_map(|(c, cell)| match &cell.value {
                Value::Error(CellError::Circ) => Some((c, Value::Number(0.0))),
                Value::Error(CellError::Spill | CellError::Calc | CellError::Python | CellError::Busy | CellError::Timeout | CellError::Blocked) => {
                    Some((c, Value::Error(CellError::Value)))
                }
                _ => None,
            })
            .collect();
        for (c, v) in changed {
            if let Some(mut cell) = sh.cell(c).cloned() {
                cell.value = v;
                sh.cells.set(c, cell);
            }
        }
    }
    wb
}

/// Save and reopen, three ways:
/// 1. as `file.open` opens a file (recalculating), then the original and the reopened workbook
///    both recalculated in full under the sequence's clock and a restarted RAND stream: both
///    digests must agree, so what was entered and what it calculates to survive;
/// 2. with the values the file holds (`DocState::from_saved`, a checkpoint loaded without
///    recalculating): `entered` must agree, and `computed` must agree with the original's values
///    as the file caches them ([`as_cached`]).
fn lossless(c: &Case) -> Result<(), String> {
    let mut s = build(c)?;
    let bytes = save(&mut s)?;
    // The cached values first, before anything recalculates the original.
    let back = reopen(&bytes, &c.seq)?;
    let (a, b) = (fingerprint(&s, DocId(0)), fingerprint(&back, DocId(0)));
    if a.entered != b.entered {
        return Err(format!("{}: entered differs after save and reopen: {}", c.name, first_diff(&s, &back)));
    }
    let cached = as_cached(&s.doc().map_err(|e| e.to_string())?.wb);
    let back_wb = &back.doc().map_err(|e| e.to_string())?.wb;
    if workbook_fingerprint(&cached).computed != b.computed {
        let diff = workbook_first_difference(&cached, back_wb).unwrap_or_else(|| "(digests differ but no item does)".into());
        return Err(format!("{}: the file's cached values differ from the workbook's: {diff}", c.name));
    }
    // Opened as File › Open opens it (recalculating), then both recalculated alike.
    let mut opened = Session::new();
    opened
        .execute("file.open", json!({"name": NAME, "base64": io::base64_encode(&bytes)}))
        .map_err(|e| format!("{}: file.open failed: {e}", c.name))?;
    opened.take_ui_requests();
    recalc_fully(&mut opened, &c.seq)?;
    recalc_fully(&mut s, &c.seq)?;
    let (a, b) = (fingerprint(&s, DocId(0)), fingerprint(&opened, DocId(0)));
    if a.entered != b.entered {
        return Err(format!("{}: entered differs after file.open: {}", c.name, first_diff(&s, &opened)));
    }
    if a.computed != b.computed {
        return Err(format!("{}: computed differs after file.open and a full recalculation: {}", c.name, first_diff(&s, &opened)));
    }
    Ok(())
}

#[test]
fn save_reload_lossless() {
    let cases = all_cases();
    let failures: Vec<String> = cases.iter().filter_map(|c| lossless(c).err()).collect();
    assert!(failures.is_empty(), "{} of {} case(s) lost something\n\n{}", failures.len(), cases.len(), failures.join("\n\n"));
}

/// The edge cases hold what they claim, so a pass is not an empty workbook passing.
#[test]
fn edge_cases_hold_what_they_claim() {
    let cases = edge_cases();
    let get = |name: &str| -> Session { build(cases.iter().find(|c| c.name == name).unwrap()).unwrap_or_else(|e| panic!("{e}")) };
    let value = |s: &Session, sheet: &str, a1: &str| -> Value {
        let wb = &s.doc().unwrap().wb;
        wb.sheet(wb.sheet_index(sheet).unwrap()).unwrap().value(CellRef::parse(a1).unwrap())
    };
    let formula = |s: &Session, sheet: &str, a1: &str| -> Option<String> {
        let wb = &s.doc().unwrap().wb;
        wb.sheet(wb.sheet_index(sheet).unwrap()).unwrap().cell(CellRef::parse(a1).unwrap()).and_then(|c| c.formula.as_ref()).map(|f| f.text.clone())
    };
    let s = get("edge-quoted-sheet-names");
    assert_eq!(value(&s, "Sheet1", "A1"), Value::Number(10.0));
    assert_eq!(formula(&s, "Sheet1", "A1").as_deref(), Some("'Q 1 Plan'!A1+'O''Brien'!A1+'2024'!A1+'A1'!A1"));
    let s = get("edge-renamed-and-deleted-sheets");
    assert_eq!(value(&s, "Sheet1", "A1"), Value::Number(10.0));
    assert_eq!(value(&s, "Sheet1", "A2"), Value::Error(gridcraft_engine::core::CellError::Ref));
    assert_eq!(value(&s, "Sheet1", "A5"), Value::Number(6.0));
    assert!(formula(&s, "Sheet1", "A1").is_some_and(|f| f.contains("'New name''s'!")), "{:?}", formula(&s, "Sheet1", "A1"));
    let s = get("edge-numbers");
    assert_eq!(value(&s, "Sheet1", "A1"), Value::Number(1e308));
    assert_eq!(value(&s, "Sheet1", "A5").as_f64().map(f64::to_bits), Some(5e-324f64.to_bits()));
    assert_eq!(value(&s, "Sheet1", "B1"), Value::Number(0.1 + 0.2));
    // The engine never stores negative zero: typed `-0` and arithmetic that gives -0 both become
    // 0. (The writer writes -0 as `0`, so if the engine ever kept -0, `save_reload_lossless`
    // would catch it here.)
    for a1 in ["A7", "B3", "B14", "B15", "B16", "B17"] {
        assert_eq!(value(&s, "Sheet1", a1).as_f64().map(f64::to_bits), Some(0), "{a1}");
    }
    let s = get("edge-empty-text-and-awkward-text");
    assert_eq!(value(&s, "Sheet1", "A1"), Value::Text("".into()));
    assert_eq!(value(&s, "Sheet1", "A9"), Value::Empty);
    let s = get("edge-merged-areas");
    assert_eq!(s.doc().unwrap().wb.sheet(0).unwrap().merges.len(), 5);
    let s = get("edge-filters");
    let wb = &s.doc().unwrap().wb;
    for sh in wb.sheets.iter() {
        let f = sh.autofilter.as_ref().unwrap_or_else(|| panic!("{} has no filter", sh.name));
        assert!(!f.criteria.is_empty(), "{} has no criteria", sh.name);
        assert!((1..7).any(|r| sh.is_row_hidden(r)), "{} hides no rows", sh.name);
    }
    let s = get("edge-defined-names");
    assert_eq!(value(&s, "Sheet1", "A1"), Value::Number(20.0));
    assert!(s.doc().unwrap().wb.names.len() >= 4);
}

// ---------------------------------------------------------------- bytes

#[test]
fn save_is_deterministic() {
    let mut failures = Vec::new();
    for c in all_cases() {
        let r = (|| -> Result<(), String> {
            let (mut a, mut b) = (build(&c)?, build(&c)?);
            let (x, y) = (save(&mut a)?, save(&mut b)?);
            if x != y {
                return Err(format!("{}: two engines in the same state wrote different bytes: {}", c.name, byte_diff(&x, &y)));
            }
            // And a second save of the same session writes the same bytes again.
            if save(&mut a)? != x {
                return Err(format!("{}: saving twice wrote different bytes", c.name));
            }
            Ok(())
        })();
        if let Err(e) = r {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn save_reload_save_is_stable() {
    let mut failures = Vec::new();
    for c in all_cases() {
        let r = (|| -> Result<(), String> {
            let mut s = build(&c)?;
            let first = save(&mut s)?;
            let mut back = reopen(&first, &c.seq)?;
            let second = save(&mut back)?;
            if first != second {
                return Err(format!("{}: write, reopen, write changed the bytes: {}", c.name, byte_diff(&first, &second)));
            }
            Ok(())
        })();
        if let Err(e) = r {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Where two .xlsx files differ: the first part whose bytes differ, and the text around the
/// first differing byte of that part.
fn byte_diff(a: &[u8], b: &[u8]) -> String {
    let parts = |bytes: &[u8]| -> Vec<(String, Vec<u8>)> {
        let Ok(mut zip) = zip_parts(bytes) else { return vec![] };
        zip.sort_by(|x, y| x.0.cmp(&y.0));
        zip
    };
    let (pa, pb) = (parts(a), parts(b));
    let names = |p: &[(String, Vec<u8>)]| p.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
    if names(&pa) != names(&pb) {
        return format!("parts {:?} vs {:?}", names(&pa), names(&pb));
    }
    for ((name, x), (_, y)) in pa.iter().zip(&pb) {
        if x != y {
            let at = x.iter().zip(y).position(|(p, q)| p != q).unwrap_or(x.len().min(y.len()));
            let around = |v: &[u8]| String::from_utf8_lossy(&v[at.saturating_sub(80)..(at + 80).min(v.len())]).into_owned();
            return format!("{name} at byte {at}:\n    {}\n  vs\n    {}", around(x), around(y));
        }
    }
    format!("the parts are equal but the packages differ ({} vs {} bytes)", a.len(), b.len())
}

/// The parts of an .xlsx package, by name.
fn zip_parts(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| e.to_string())?;
        out.push((f.name().to_string(), v));
    }
    Ok(out)
}

/// Known, left for G4: opening a file fits every row without a stored height to its cells
/// (`io::open_bytes`, as Excel does), but the live engine fits a row only when a cell in it is
/// edited or formatted. A wrapped formula whose text grows because a cell it reads changed keeps
/// its row's height live, and gets a taller row after a save and reopen. (Seed 3996 at 60 steps
/// meets the same through a circular reference: `#CIRC!` is cached as 0, which a long-date
/// format shows as a long wrapped text.) A checkpoint loaded without re-fitting rows would keep
/// the heights as written. Run with `--ignored` to see it.
#[test]
#[ignore = "known loss for G4: opening re-fits rows the live engine left alone"]
fn known_loss_reopen_refits_rows_from_values() {
    let steps = vec![
        step("home.wrapText", json!({"sheet": "Sheet1", "range": "B1", "on": true})),
        set("Sheet1", "B1", "=A3"),
        set("Sheet1", "A3", "a long line of text that wraps in a narrow column and goes on and on and on"),
    ];
    let mut cases = vec![case("wrapped-formula-grows", 1, steps)];
    cases.push(Case { name: "seed 3996".into(), seq: generate(3_996, 60) });
    let failures: Vec<String> = cases.iter().filter_map(|c| lossless(c).err()).collect();
    assert!(
        failures.is_empty(),
        "{}",
        failures.join(
            "

"
        )
    );
}
