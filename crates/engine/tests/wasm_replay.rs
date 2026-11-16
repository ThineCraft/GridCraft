//! Native-versus-WebAssembly replay for the co-authoring proofs.
//!
//! Two records, both written natively by ignored tests and compared by wasm32 tests under Node:
//!
//! - `fixtures/coauthor/native-digests.json`: the 26 command-sequence fixtures, embedded
//!   byte-for-byte, with each step's outcome, both fingerprints and the canonical walk.
//! - `fixtures/coauthor-maths/native-maths.json`: a grid of maths and number-display formulas
//!   over awkward inputs (the platform maths library differs: the C runtime natively, Rust's
//!   libm port on wasm32). Every computed difference there must be listed, with its ULP
//!   distance, in `fixtures/coauthor-maths/allowed-differences.json`; a new or vanished one fails.
//!
//! A computed difference is reported with every function in the formula, the arguments as this
//! engine evaluates them, the precedent cells' values on both sides, the ULP distance, and
//! whether it is a root (none of its precedents differ) or follows from another difference.

#[path = "support/sequences.rs"]
mod sequences;

use std::collections::BTreeMap;

use gridcraft_engine::Session;
use gridcraft_engine::core::{CellRef, RangeRef, Value};
use gridcraft_engine::fingerprint::{DocId, FingerprintItem, fingerprint, fingerprint_items};
use gridcraft_engine::formula::{Expr, SheetSel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use sequences::{DEFAULT_NOW, Sequence, Step, apply, fresh_session};

const NATIVE_DIGESTS: &str = include_str!("fixtures/coauthor/native-digests.json");
const NATIVE_MATHS: &str = include_str!("fixtures/coauthor-maths/native-maths.json");
const ALLOWED_MATHS_DIFFERENCES: &str = include_str!("fixtures/coauthor-maths/allowed-differences.json");

const FIXTURES: [(&str, &str); 26] = [
    ("hand-1-arithmetic-across-sheets.json", include_str!("fixtures/coauthor/hand-1-arithmetic-across-sheets.json")),
    ("hand-2-insert-rows-moves-references.json", include_str!("fixtures/coauthor/hand-2-insert-rows-moves-references.json")),
    ("hand-3-deleted-column-gives-ref-errors.json", include_str!("fixtures/coauthor/hand-3-deleted-column-gives-ref-errors.json")),
    ("hand-4-renamed-then-deleted-sheet.json", include_str!("fixtures/coauthor/hand-4-renamed-then-deleted-sheet.json")),
    ("hand-5-formats-outlive-cleared-content.json", include_str!("fixtures/coauthor/hand-5-formats-outlive-cleared-content.json")),
    ("hand-6-text-lookup-and-logic.json", include_str!("fixtures/coauthor/hand-6-text-lookup-and-logic.json")),
    ("random-001.json", include_str!("fixtures/coauthor/random-001.json")),
    ("random-002.json", include_str!("fixtures/coauthor/random-002.json")),
    ("random-003.json", include_str!("fixtures/coauthor/random-003.json")),
    ("random-004.json", include_str!("fixtures/coauthor/random-004.json")),
    ("random-005.json", include_str!("fixtures/coauthor/random-005.json")),
    ("random-006.json", include_str!("fixtures/coauthor/random-006.json")),
    ("random-007.json", include_str!("fixtures/coauthor/random-007.json")),
    ("random-008.json", include_str!("fixtures/coauthor/random-008.json")),
    ("random-009.json", include_str!("fixtures/coauthor/random-009.json")),
    ("random-010.json", include_str!("fixtures/coauthor/random-010.json")),
    ("random-011.json", include_str!("fixtures/coauthor/random-011.json")),
    ("random-012.json", include_str!("fixtures/coauthor/random-012.json")),
    ("random-013.json", include_str!("fixtures/coauthor/random-013.json")),
    ("random-014.json", include_str!("fixtures/coauthor/random-014.json")),
    ("random-015.json", include_str!("fixtures/coauthor/random-015.json")),
    ("random-016.json", include_str!("fixtures/coauthor/random-016.json")),
    ("random-017.json", include_str!("fixtures/coauthor/random-017.json")),
    ("random-018.json", include_str!("fixtures/coauthor/random-018.json")),
    ("random-019.json", include_str!("fixtures/coauthor/random-019.json")),
    ("random-020.json", include_str!("fixtures/coauthor/random-020.json")),
];

/// Most computed differences printed per fixture (all of them are still counted).
const SHOWN_DIFFERENCES: usize = 20;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeDigests {
    fixtures: Vec<NativeFixture>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeFixture {
    name: String,
    /// The steps the engine rejected, so a command refused on one platform only is named.
    failed_steps: Vec<StepFailure>,
    entered: String,
    computed: String,
    items: Vec<NativeItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct StepFailure {
    step: usize,
    id: String,
    error: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct NativeItem {
    stream: String,
    key: String,
    canonical: String,
}

/// The maths grid's native record. The computed values depend on the platform's maths library,
/// so the record names the platform that wrote it.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct MathsRecord {
    platform: String,
    entered: String,
    computed: String,
    items: Vec<NativeItem>,
}

/// One computed cell whose value differs between the native record and this engine.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Difference {
    cell: String,
    formula: String,
    functions: Vec<String>,
    native: String,
    wasm: String,
    /// Distance in units in the last place, when both values are numbers.
    ulp: Option<u64>,
    /// None of the cells the formula refers to differ: this cell is where the difference starts.
    root: bool,
    /// Each call in the formula with its arguments as this engine evaluates them.
    arguments: Vec<String>,
    /// The cells the formula refers to (the first few of each range), with both sides' values.
    precedents: Vec<String>,
}

struct Replayed {
    session: Session,
    failed_steps: Vec<StepFailure>,
}

fn run_steps(name: &str, seq: &Sequence) -> Result<Replayed, String> {
    let mut session = fresh_session(seq);
    let mut failed_steps = Vec::new();
    for (index, step) in seq.steps.iter().enumerate() {
        if let Err(error) = apply(&mut session, step) {
            if error.contains("panic") {
                return Err(format!("{name}: step {index} `{}` panicked: {error}", step.id));
            }
            failed_steps.push(StepFailure { step: index, id: step.id.clone(), error });
        }
    }
    Ok(Replayed { session, failed_steps })
}

fn replay(name: &str, text: &str) -> Result<Replayed, String> {
    let seq = Sequence::from_text(text).map_err(|e| format!("{name}: invalid fixture: {e}"))?;
    run_steps(name, &seq)
}

fn native_item(item: FingerprintItem) -> NativeItem {
    let canonical = if item.stream == "entered" {
        let digest = Sha256::digest(item.canonical.as_bytes());
        let hex = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        format!("sha256 {hex}")
    } else {
        item.canonical
    };
    NativeItem { stream: item.stream.to_string(), key: item.key, canonical }
}

fn recorded_items(session: &Session) -> Vec<NativeItem> {
    fingerprint_items(session, DocId(0)).into_iter().map(native_item).collect()
}

// ---------------------------------------------------------------- reporting differences

/// The bits of a canonical number (`num 3ff0000000000000 (1)`).
fn number_bits(canonical: &str) -> Option<u64> {
    let hex = canonical.strip_prefix("num ")?.get(..16)?;
    u64::from_str_radix(hex, 16).ok()
}

/// How many representable doubles lie between two canonical numbers.
fn ulp_distance(a: &str, b: &str) -> Option<u64> {
    let ordered = |bits: u64| -> i128 {
        let magnitude = i128::from(bits & !(1u64 << 63));
        if bits >> 63 == 1 { -magnitude } else { magnitude }
    };
    let (x, y) = (number_bits(a)?, number_bits(b)?);
    u64::try_from((ordered(x) - ordered(y)).unsigned_abs()).ok()
}

/// A value as the fingerprint writes it (numbers by their bits), arrays summarised.
fn value_text(v: &Value) -> String {
    match v {
        Value::Empty => "empty".into(),
        Value::Number(n) => format!("num {:016x} ({n})", n.to_bits()),
        Value::Text(t) => format!("text {}", serde_json::Value::String(t.to_string())),
        Value::Bool(b) => format!("bool {b}"),
        Value::Error(e) => format!("error {} {}", e.code(), e.as_str()),
        Value::Array(a) => {
            let first: Vec<String> = a.iter().take(8).map(value_text).collect();
            let more = if a.rows * a.cols > 8 { ", …" } else { "" };
            format!("array {}x{} [{}{more}]", a.rows, a.cols, first.join("; "))
        }
    }
}

/// A walk key (`Sheet1!B2`, `Sheet1!B2 spilled`) as a sheet index and cell of `session`.
fn locate(session: &Session, key: &str) -> Option<(usize, CellRef)> {
    let key = key.strip_suffix(" spilled").unwrap_or(key);
    let (sheet_name, cell_name) = key.rsplit_once('!')?;
    let wb = &session.documents().first()?.wb;
    Some((wb.sheet_index(sheet_name)?, CellRef::parse(cell_name)?))
}

/// The areas a formula refers to, with their sheets resolved.
fn referenced_areas(session: &Session, sheet: usize, expr: &Expr) -> Vec<(usize, RangeRef)> {
    let Some(document) = session.documents().first() else { return Vec::new() };
    let mut areas = Vec::new();
    for reference in expr.references() {
        let range = reference.range();
        match &reference.sheet {
            SheetSel::Current => areas.push((sheet, range)),
            SheetSel::Named(name) => {
                if let Some(index) = document.wb.sheet_index(name) {
                    areas.push((index, range));
                }
            }
            SheetSel::Span(a, b) => {
                if let (Some(i), Some(j)) = (document.wb.sheet_index(a), document.wb.sheet_index(b)) {
                    for index in i.min(j)..=i.max(j) {
                        areas.push((index, range));
                    }
                }
            }
        }
    }
    areas
}

/// Every computed item whose value differs between the native record and `actual`, roots first,
/// each other one in walk order.
fn computed_differences(expected: &[NativeItem], actual: &[NativeItem], session: &Session) -> Vec<Difference> {
    let native: BTreeMap<&str, &str> = expected.iter().filter(|i| i.stream == "computed").map(|i| (i.key.as_str(), i.canonical.as_str())).collect();
    let wasm: BTreeMap<&str, &str> = actual.iter().filter(|i| i.stream == "computed").map(|i| (i.key.as_str(), i.canonical.as_str())).collect();
    let mut keys: Vec<&str> = actual.iter().filter(|i| i.stream == "computed").map(|i| i.key.as_str()).collect();
    keys.extend(expected.iter().filter(|i| i.stream == "computed" && !wasm.contains_key(i.key.as_str())).map(|i| i.key.as_str()));
    let differing: Vec<&str> = keys.into_iter().filter(|k| native.get(k) != wasm.get(k)).collect();
    let differing_cells: Vec<(usize, CellRef)> = differing.iter().filter_map(|k| locate(session, k)).collect();
    let Some(document) = session.documents().first() else { return Vec::new() };
    let wb = &document.wb;

    let mut out: Vec<Difference> = differing
        .iter()
        .map(|key| {
            let native_value = native.get(key).map_or_else(|| "absent".to_string(), |v| v.to_string());
            let wasm_value = wasm.get(key).map_or_else(|| "absent".to_string(), |v| v.to_string());
            let mut difference = Difference {
                cell: key.to_string(),
                formula: String::new(),
                functions: Vec::new(),
                ulp: ulp_distance(&native_value, &wasm_value),
                native: native_value,
                wasm: wasm_value,
                root: true,
                arguments: Vec::new(),
                precedents: Vec::new(),
            };
            let Some((sheet, cell)) = locate(session, key) else { return difference };
            let Some(expr) = wb.sheet(sheet).and_then(|s| s.cell(cell)).and_then(|c| c.formula.as_ref()).and_then(|f| f.with_expr(Clone::clone))
            else {
                return difference;
            };
            difference.formula = gridcraft_engine::formula::print(&expr);
            expr.walk(&mut |e| {
                if let Expr::Call(name, args) = e {
                    if !difference.functions.contains(name) {
                        difference.functions.push(name.clone());
                    }
                    let values: Vec<String> =
                        args.iter().map(|a| value_text(&gridcraft_engine::calc::recalc::evaluate_expr(wb, sheet, cell, a))).collect();
                    difference.arguments.push(format!("{} <- [{}]", gridcraft_engine::formula::print(e), values.join(", ")));
                }
            });
            let areas = referenced_areas(session, sheet, &expr);
            difference.root = !differing_cells.iter().any(|(s, c)| areas.iter().any(|(area_sheet, range)| area_sheet == s && range.contains(*c)));
            for (area_sheet, range) in &areas {
                let Some(sheet_name) = wb.sheet(*area_sheet).map(|s| s.name.clone()) else { continue };
                for c in range.iter().take(8) {
                    if difference.precedents.len() >= 12 {
                        break;
                    }
                    let k = format!("{sheet_name}!{}", c.a1());
                    let n = native.get(k.as_str()).copied().unwrap_or("empty");
                    let w = wasm.get(k.as_str()).copied().unwrap_or("empty");
                    difference.precedents.push(if n == w { format!("{k}: {n}") } else { format!("{k}: native {n}, wasm {w}") });
                }
            }
            difference
        })
        .collect();
    // Roots first; `sort_by_key` is stable, so walk order is kept within each group.
    out.sort_by_key(|d| !d.root);
    out
}

fn describe(d: &Difference) -> String {
    let ulp = d.ulp.map_or_else(String::new, |u| format!(", {u} ulp"));
    let mut s = format!("computed {} ({}): native {} != wasm {}{ulp}", d.cell, if d.root { "root" } else { "follows a precedent" }, d.native, d.wasm);
    if !d.formula.is_empty() {
        s.push_str(&format!("\n      formula ={} functions [{}]", d.formula, d.functions.join(", ")));
    }
    for a in &d.arguments {
        s.push_str(&format!("\n      call {a}"));
    }
    for p in &d.precedents {
        s.push_str(&format!("\n      precedent {p}"));
    }
    s
}

/// The first entered item that differs, with what this engine reads there (the record stores
/// entered items hashed).
fn entered_difference(expected: &[NativeItem], actual: &[NativeItem], session: &Session) -> Option<String> {
    let mut left = expected.iter().filter(|item| item.stream == "entered");
    let mut right = actual.iter().filter(|item| item.stream == "entered");
    loop {
        return Some(match (left.next(), right.next()) {
            (None, None) => return None,
            (Some(item), None) => format!("entered: {} ({}) only in native", item.key, item.canonical),
            (None, Some(item)) => format!("entered: {} ({}) only in wasm", item.key, item.canonical),
            (Some(a), Some(b)) if a.key != b.key => {
                format!("entered: native has {} ({}) where wasm has {} ({})", a.key, a.canonical, b.key, b.canonical)
            }
            (Some(a), Some(b)) if a.canonical != b.canonical => {
                let reads = fingerprint_items(session, DocId(0))
                    .into_iter()
                    .find(|item| item.stream == "entered" && item.key == b.key)
                    .map(|item| format!("; wasm reads {}", item.canonical))
                    .unwrap_or_default();
                format!("entered {}: native {} != wasm {}{reads}", a.key, a.canonical, b.canonical)
            }
            (Some(_), Some(_)) => continue,
        });
    }
}

/// Everything that differs: the first entered item, then every computed difference (the first
/// [`SHOWN_DIFFERENCES`], roots first).
fn report(expected: &[NativeItem], actual: &[NativeItem], session: &Session) -> String {
    let mut lines = Vec::new();
    if let Some(entered) = entered_difference(expected, actual, session) {
        lines.push(entered);
    }
    let computed = computed_differences(expected, actual, session);
    let roots = computed.iter().filter(|d| d.root).count();
    if !computed.is_empty() {
        lines.push(format!("{} computed difference(s), {roots} root(s)", computed.len()));
    }
    lines.extend(computed.iter().take(SHOWN_DIFFERENCES).map(describe));
    if lines.is_empty() {
        lines.push("digests differ but the canonical walks agree".to_string());
    }
    lines.join("\n    ")
}

fn step_differences(native: &[StepFailure], wasm: &[StepFailure]) -> Vec<String> {
    let mut steps: Vec<usize> = native.iter().chain(wasm).map(|f| f.step).collect();
    steps.sort_unstable();
    steps.dedup();
    let mut out = Vec::new();
    for step in steps {
        let n = native.iter().find(|f| f.step == step);
        let w = wasm.iter().find(|f| f.step == step);
        if n != w {
            let side = |f: Option<&StepFailure>| f.map_or_else(|| "ok".to_string(), |f| format!("error {}", f.error));
            let id = n.or(w).map_or("", |f| f.id.as_str());
            out.push(format!("step {step} `{id}`: native {}, wasm {}", side(n), side(w)));
        }
    }
    out
}

// ---------------------------------------------------------------- the 26 fixtures

#[cfg(not(target_arch = "wasm32"))]
fn current_native_digests() -> Result<NativeDigests, String> {
    let mut fixtures = Vec::with_capacity(FIXTURES.len());
    for (name, text) in FIXTURES {
        let replayed = replay(name, text)?;
        let (entered, computed) = fingerprint(&replayed.session, DocId(0)).hex();
        let items = recorded_items(&replayed.session);
        fixtures.push(NativeFixture { name: name.to_string(), failed_steps: replayed.failed_steps, entered, computed, items });
    }
    Ok(NativeDigests { fixtures })
}

#[cfg(not(target_arch = "wasm32"))]
fn pretty(value: &impl Serialize) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(not(target_arch = "wasm32"))]
fn write_fixture_file(relative: &str, text: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::write(&path, text).expect("the record is written");
    eprintln!("wrote {}", path.display());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "writes the native reference file"]
fn write_native_digests() {
    let digests = current_native_digests().expect("native fixture replay succeeds");
    write_fixture_file("tests/fixtures/coauthor/native-digests.json", &pretty(&digests));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn native_digests_are_current() {
    const HINT: &str = "run the ignored write_native_digests test to refresh the native reference";
    let current = current_native_digests().expect("native fixture replay succeeds");
    let stored: NativeDigests = serde_json::from_str(NATIVE_DIGESTS).unwrap_or_else(|e| panic!("native-digests.json is unreadable ({e}); {HINT}"));
    let names = |d: &NativeDigests| d.fixtures.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&stored), names(&current), "the recorded fixtures differ; {HINT}");
    for (s, c) in stored.fixtures.iter().zip(&current.fixtures) {
        assert_eq!(s.failed_steps, c.failed_steps, "{}: step outcomes changed; {HINT}", s.name);
        let changed =
            s.items.iter().zip(&c.items).find(|(a, b)| a != b).map(|(a, b)| format!("{} {}: {} -> {}", a.stream, a.key, a.canonical, b.canonical));
        assert!(
            changed.is_none() && s.items.len() == c.items.len(),
            "{}: {}; {HINT}",
            s.name,
            changed.unwrap_or_else(|| "the item count changed".into())
        );
        assert_eq!((&s.entered, &s.computed), (&c.entered, &c.computed), "{}: digests changed; {HINT}", s.name);
    }
    // The file's exact text too, so a hand edit or reformat is caught.
    assert!(NATIVE_DIGESTS.replace("\r\n", "\n") == pretty(&current), "native-digests.json is not as the writer writes it; {HINT}");
}

/// The embedded list must name every sequence in the fixtures folder, so a fixture added there
/// is replayed in WebAssembly too, not only by the native tests that read the folder.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn embedded_fixtures_match_the_folder() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coauthor");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("the fixtures folder is readable")
        .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
        .filter(|n| n.ends_with(".json") && n != "native-digests.json")
        .collect();
    on_disk.sort();
    let embedded: Vec<String> = FIXTURES.iter().map(|(name, _)| name.to_string()).collect();
    assert_eq!(embedded, on_disk, "add the new fixture to FIXTURES in tests/wasm_replay.rs and rewrite the native digests");
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test::wasm_bindgen_test]
fn wasm_matches_native_fixtures() {
    let expected: NativeDigests = serde_json::from_str(NATIVE_DIGESTS).expect("native digest file is valid JSON");
    let mut failures = Vec::new();
    for (name, text) in FIXTURES {
        let Some(native) = expected.fixtures.iter().find(|fixture| fixture.name == name) else {
            failures.push(format!("{name}: missing native digest"));
            continue;
        };
        let replayed = match replay(name, text) {
            Ok(replayed) => replayed,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        let mut lines = step_differences(&native.failed_steps, &replayed.failed_steps);
        let (entered, computed) = fingerprint(&replayed.session, DocId(0)).hex();
        if entered != native.entered || computed != native.computed {
            lines.push(report(&native.items, &recorded_items(&replayed.session), &replayed.session));
        }
        if !lines.is_empty() {
            failures.push(format!("{name}:\n    {}", lines.join("\n    ")));
        }
    }
    assert!(failures.is_empty(), "{} fixture(s) differ between native and wasm:\n{}", failures.len(), failures.join("\n"));
}

// ---------------------------------------------------------------- the maths grid

/// The inputs, one per row of column A: everyday values, domain edges, extremes, and each power
/// of ten with its neighbours one ULP either side (where number display switches digits).
fn maths_inputs() -> Vec<f64> {
    let mut inputs = vec![
        0.1,
        0.5,
        1.0 / 3.0,
        0.7,
        0.999999,
        2.0,
        std::f64::consts::E,
        std::f64::consts::FRAC_PI_4,
        3.7,
        10.0,
        123456.789,
        1e-300,
        1e300,
        -0.5,
        -2.5,
        0.1 + 0.2,
        999999999999999.4,
    ];
    for exponent in [-5, -3, -1, 1, 2, 3, 5, 10, 15] {
        // Parsed, not computed with powi, so the inputs cannot depend on the platform.
        let power: f64 = format!("1e{exponent}").parse().unwrap_or(f64::NAN);
        inputs.extend([f64::from_bits(power.to_bits() - 1), power, f64::from_bits(power.to_bits() + 1)]);
    }
    inputs
}

/// The formulas in columns B onwards; `@` stands for the row's input cell.
const MATHS_FORMULAS: [&str; 36] = [
    "LN(@)",
    "LOG(@)",
    "LOG(@,2)",
    "LOG10(@)",
    "EXP(@)",
    "SQRT(@)",
    "POWER(@,0.5)",
    "POWER(@,1/3)",
    "POWER(@,2.5)",
    "POWER(@,-1.7)",
    "@^0.37",
    "POWER(2,@)",
    "SIN(@)",
    "COS(@)",
    "TAN(@)",
    "ASIN(@)",
    "ACOS(@)",
    "ATAN(@)",
    "ATAN2(@,1.5)",
    "ATAN2(1.5,@)",
    "SINH(@)",
    "COSH(@)",
    "TANH(@)",
    "ASINH(@)",
    "ACOSH(@)",
    "ATANH(@)",
    "@&\"\"",
    "LN(@)&\"\"",
    "EXP(@)&\"\"",
    "TEXT(@,\"0.000000000000000E+00\")",
    "TEXT(@,\"General\")",
    "TEXT(@,\"#,##0.00\")",
    "TEXT(@,\"0.0000000000\")",
    "ROUND(@,3)",
    "ROUND(LN(@),4)",
    "LOG10(@)&\"\"",
];

fn maths_sequence() -> Sequence {
    let mut steps = Vec::new();
    for (row, x) in maths_inputs().into_iter().enumerate() {
        let r = row + 1;
        steps.push(Step::new("cell.set", json!({"sheet": "Sheet1", "cell": format!("A{r}"), "input": format!("{x:?}")})));
        for (col, formula) in MATHS_FORMULAS.iter().enumerate() {
            let cell = format!("{}{r}", sequences::letters(u32::try_from(col + 1).unwrap_or(0)));
            let input = format!("={}", formula.replace('@', &format!("$A{r}")));
            steps.push(Step::new("cell.set", json!({"sheet": "Sheet1", "cell": cell, "input": input})));
        }
    }
    Sequence { seed: 1, now: DEFAULT_NOW, steps }
}

fn maths_session() -> Session {
    let replayed = run_steps("maths grid", &maths_sequence()).expect("the maths grid replays");
    assert!(replayed.failed_steps.is_empty(), "maths grid steps failed: {:?}", replayed.failed_steps);
    replayed.session
}

#[cfg(not(target_arch = "wasm32"))]
fn platform() -> String {
    let env = if cfg!(target_env = "msvc") {
        "msvc"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else {
        "other"
    };
    format!("{}-{}-{env}", std::env::consts::ARCH, std::env::consts::OS)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "writes the native maths record (its computed values depend on this platform's maths library)"]
fn write_native_maths() {
    let session = maths_session();
    let (entered, computed) = fingerprint(&session, DocId(0)).hex();
    let record = MathsRecord { platform: platform(), entered, computed, items: recorded_items(&session) };
    write_fixture_file("tests/fixtures/coauthor-maths/native-maths.json", &pretty(&record));
}

/// Every input reaches its cell with exactly the intended bits, on every platform.
#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn maths_inputs_are_exact() {
    let session = maths_session();
    let sheet = session.documents().first().and_then(|d| d.wb.sheet(0)).expect("the grid has a sheet");
    for (row, x) in maths_inputs().into_iter().enumerate() {
        let cell = CellRef::new(u32::try_from(row).unwrap_or(0), 0);
        let stored = sheet.cell(cell).map(|c| c.value.clone());
        assert_eq!(stored.as_ref().map(value_text), Some(value_text(&Value::Number(x))), "input {} ({x:?})", cell.a1());
    }
}

/// The grid's entered content does not depend on the platform, so its record is checked here;
/// the computed values are only compared in wasm, against the platform that wrote the record.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn maths_entered_record_is_current() {
    let record: MathsRecord = serde_json::from_str(NATIVE_MATHS).expect("native-maths.json is valid JSON");
    let (entered, _) = fingerprint(&maths_session(), DocId(0)).hex();
    assert_eq!(record.entered, entered, "run the ignored write_native_maths test to refresh the maths record");
}

/// Each allowed difference names a cell of the record with the record's own value, so rewriting
/// the record (on another platform, or after an engine change) cannot leave a stale list behind.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn allowed_maths_differences_match_the_record() {
    let record: MathsRecord = serde_json::from_str(NATIVE_MATHS).expect("native-maths.json is valid JSON");
    let allowed: Vec<Difference> = serde_json::from_str(ALLOWED_MATHS_DIFFERENCES).expect("allowed-differences.json is valid JSON");
    for d in &allowed {
        let recorded = record.items.iter().find(|i| i.stream == "computed" && i.key == d.cell).map(|i| i.canonical.as_str());
        assert_eq!(recorded, Some(d.native.as_str()), "allowed difference {} does not match the record", d.cell);
        assert_ne!(d.native, d.wasm, "allowed difference {} has equal values", d.cell);
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test::wasm_bindgen_test]
fn wasm_maths_matches_native_within_the_allowed_list() {
    let record: MathsRecord = serde_json::from_str(NATIVE_MATHS).expect("native-maths.json is valid JSON");
    let allowed: Vec<Difference> = serde_json::from_str(ALLOWED_MATHS_DIFFERENCES).expect("allowed-differences.json is valid JSON");
    let session = maths_session();
    let actual = recorded_items(&session);
    let (entered, _) = fingerprint(&session, DocId(0)).hex();
    if let Some(difference) = entered_difference(&record.items, &actual, &session) {
        panic!("entered content differs from the {} record: {difference}", record.platform);
    }
    assert_eq!(entered, record.entered, "entered digests differ though every entered item agrees");
    let found = computed_differences(&record.items, &actual, &session);
    let max_ulp = found.iter().filter_map(|d| d.ulp).max();
    assert!(
        found == allowed,
        "computed differences from the {} record changed: {} found ({} roots, max {max_ulp:?} ulp), {} allowed\nnot allowed:\n{}\nallowed but not found:\n{}\nfound, as allowed-differences.json would list them:\n{}",
        record.platform,
        found.len(),
        found.iter().filter(|d| d.root).count(),
        allowed.len(),
        found.iter().filter(|d| !allowed.contains(d)).map(describe).collect::<Vec<_>>().join("\n"),
        allowed.iter().filter(|d| !found.contains(d)).map(describe).collect::<Vec<_>>().join("\n"),
        serde_json::to_string_pretty(&found).unwrap_or_default()
    );
}

// ---------------------------------------------------------------- the reports themselves

#[cfg(not(target_arch = "wasm32"))]
fn diagnostic_session() -> Session {
    // A1 depends on B2, which the walk visits later; B2 nests LN inside ROUND.
    let steps = vec![
        Step::new("cell.set", json!({"sheet": "Sheet1", "cell": "A1", "input": "=B2*2"})),
        Step::new("cell.set", json!({"sheet": "Sheet1", "cell": "B2", "input": "=ROUND(LN(C3),4)"})),
        Step::new("cell.set", json!({"sheet": "Sheet1", "cell": "C3", "input": "0.5"})),
    ];
    run_steps("diagnostic", &Sequence { seed: 1, now: DEFAULT_NOW, steps }).expect("the diagnostic replays").session
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn difference_reports_root_functions_and_inputs() {
    let session = diagnostic_session();
    let native = recorded_items(&session);
    let mut wasm = native.clone();
    for item in wasm.iter_mut().filter(|i| i.stream == "computed" && (i.key == "Sheet1!A1" || i.key == "Sheet1!B2")) {
        let bits = number_bits(&item.canonical).expect("both cells are numbers");
        item.canonical = format!("num {:016x} (x)", bits + 1);
    }
    let found = computed_differences(&native, &wasm, &session);
    assert_eq!(found.iter().map(|d| (d.cell.as_str(), d.root)).collect::<Vec<_>>(), [("Sheet1!B2", true), ("Sheet1!A1", false)]);
    let root = &found[0];
    assert_eq!(root.ulp, Some(1));
    assert_eq!(root.functions, ["ROUND", "LN"]);
    assert_eq!(root.formula, "ROUND(LN(C3),4)");
    // LN's argument is C3's value, ROUND's first argument LN's result.
    assert!(root.arguments.iter().any(|a| a.starts_with("LN(C3) <- [num 3fe0000000000000 (0.5)]")), "{:?}", root.arguments);
    assert!(root.arguments.iter().any(|a| a.starts_with("ROUND(LN(C3),4) <- [num ") && a.contains("(-0.6931471805599453)")), "{:?}", root.arguments);
    assert_eq!(root.precedents, ["Sheet1!C3: num 3fe0000000000000 (0.5)"]);
    assert!(found[1].precedents[0].starts_with("Sheet1!B2: native num "), "{:?}", found[1].precedents);
    let text = report(&native, &wasm, &session);
    assert!(text.starts_with("2 computed difference(s), 1 root(s)"), "{text}");
    assert!(text.contains("computed Sheet1!B2 (root)"), "{text}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn entered_difference_names_what_wasm_reads() {
    let session = diagnostic_session();
    let native = recorded_items(&session);
    let mut wasm = native.clone();
    let item = wasm.iter_mut().find(|i| i.stream == "entered" && i.key == "Sheet1!B2").expect("B2 is entered");
    item.canonical = "sha256 0".to_string();
    let text = report(&native, &wasm, &session);
    assert!(text.contains("entered Sheet1!B2"), "{text}");
    assert!(text.contains("wasm reads formula \"ROUND(LN(C3),4)\""), "{text}");
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn ulp_distance_counts_across_zero() {
    let num = |x: f64| format!("num {:016x} ({x})", x.to_bits());
    assert_eq!(ulp_distance(&num(1.0), &num(f64::from_bits(1.0f64.to_bits() + 3))), Some(3));
    assert_eq!(ulp_distance(&num(f64::from_bits(1)), &num(-f64::from_bits(1))), Some(2));
    assert_eq!(ulp_distance(&num(1.0), "text \"1\""), None);
}

#[cfg_attr(not(target_arch = "wasm32"), test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
fn step_differences_name_one_sided_failures() {
    let failure = StepFailure { step: 3, id: "cell.set".into(), error: "bad".into() };
    assert_eq!(step_differences(std::slice::from_ref(&failure), std::slice::from_ref(&failure)), Vec::<String>::new());
    assert_eq!(step_differences(&[], &[failure]), ["step 3 `cell.set`: native ok, wasm error bad"]);
}
