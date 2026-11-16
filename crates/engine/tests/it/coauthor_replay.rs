//! Two-engine replay of shared commands (co-authoring plan G1): two copies of the engine, given
//! the same ordered commands, end in the same workbook, checked by fingerprint after every step.
//!
//! - `two_engines_agree_on_random_sequences`: seeded random sequences of the candidate shared
//!   commands (`tests/support/sequences.rs`) plus the checked-in fixtures, applied to two fresh
//!   sessions. Both must give the same outcome (result or error) and the same `entered` and
//!   `computed` digests after every step. A mismatch names the seed, the step and the first
//!   differing cell.
//! - `replay_matches_hand_written_expectations`: six hand-written sequences whose final cells and
//!   values are known independently, so a pass can't come from both engines being wrong alike.
//! - `fixtures_round_trip_as_json`: every fixture reads, writes back byte for byte, and the
//!   generated ones match what the generator makes today.
//! - `volatile_functions_agree_with_fixed_now_and_seed`: NOW, TODAY, RAND and RANDBETWEEN under
//!   the sequence's clock and seed.
//!
//! Knobs: `GRIDCRAFT_COAUTHOR_SEEDS` (default 500), `GRIDCRAFT_COAUTHOR_STEPS` (default 60),
//! `GRIDCRAFT_COAUTHOR_SEED=17` replays one seed, `GRIDCRAFT_COAUTHOR_FIRST` starts the seed range
//! elsewhere (seeds `FIRST..FIRST+SEEDS`). `GRIDCRAFT_COAUTHOR_WRITE_FIXTURES=1` rewrites the
//! fixtures from the generator and the hand-written sequences. The long run:
//! `GRIDCRAFT_COAUTHOR_SEEDS=10000 cargo test --release -p gridcraft-engine --test it coauthor_replay::two_engines`.

use std::path::PathBuf;

use gridcraft_engine::Session;
use gridcraft_engine::core::{CellError, CellRef, Value};
use gridcraft_engine::fingerprint::{DocId, fingerprint, first_difference};
use gridcraft_engine::model::{BorderStyle, Color, Fill, HAlign, Style};
use serde_json::json;

use crate::sequences::{DEFAULT_NOW, SHARED_COMMANDS, STRUCTURAL_COMMANDS, Sequence, Step, apply, fresh_session, generate};

/// Random steps per generated sequence (after the steps that add its starting sheets).
const STEPS: usize = 60;
/// How many generated fixtures are checked in (seeds 1..=20).
const FIXTURES: u64 = 20;

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("coauthor")
}

// ---------------------------------------------------------------- the replay

/// What one replay saw, for the error-rate check.
#[derive(Default)]
struct Tally {
    steps: usize,
    errors: usize,
    structural: usize,
}

/// Applies `seq` to two fresh sessions, comparing outcomes and fingerprints after every step.
fn replay_two(name: &str, seq: &Sequence, tally: &mut Tally) -> Result<(Session, Session), String> {
    let mut a = fresh_session(seq);
    let mut b = fresh_session(seq);
    let doc = DocId(0);
    if fingerprint(&a, doc) != fingerprint(&b, doc) {
        return Err(format!("{name}: the fresh sessions differ"));
    }
    for (i, step) in seq.steps.iter().enumerate() {
        if !SHARED_COMMANDS.contains(&step.id.as_str()) {
            return Err(format!("{name}: step {i} `{}` is not a shared command", step.id));
        }
        let ra = apply(&mut a, step);
        let rb = apply(&mut b, step);
        tally.steps += 1;
        tally.errors += usize::from(ra.is_err());
        tally.structural += usize::from(STRUCTURAL_COMMANDS.contains(&step.id.as_str()));
        let at = || format!("{name}, step {i} of {}: `{}` {}", seq.steps.len(), step.id, step.params);
        if let Err(e) = &ra
            && e.contains("panic")
        {
            return Err(format!("{}\n  panicked: {e}", at()));
        }
        if ra != rb {
            return Err(format!("{}\n  outcomes differ: {ra:?} vs {rb:?}", at()));
        }
        if fingerprint(&a, doc) != fingerprint(&b, doc) {
            let diff = first_difference(&a, &b, doc).unwrap_or_else(|| "(digests differ but no item does)".into());
            return Err(format!("{}\n  fingerprints differ: {diff}", at()));
        }
    }
    Ok((a, b))
}

#[test]
fn two_engines_agree_on_random_sequences() {
    let steps = env_num("GRIDCRAFT_COAUTHOR_STEPS", STEPS as u64) as usize;
    let seeds: Vec<u64> = match std::env::var("GRIDCRAFT_COAUTHOR_SEED").ok().and_then(|v| v.trim().parse().ok()) {
        Some(one) => vec![one],
        None => {
            let first = env_num("GRIDCRAFT_COAUTHOR_FIRST", 1_000);
            (first..first + env_num("GRIDCRAFT_COAUTHOR_SEEDS", 500)).collect()
        }
    };
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for &seed in &seeds {
        let seq = generate(seed, steps);
        if let Err(e) = replay_two(&format!("seed {seed}"), &seq, &mut tally) {
            failures.push(format!(
                "{e}\n  replay: GRIDCRAFT_COAUTHOR_SEED={seed} GRIDCRAFT_COAUTHOR_STEPS={steps} cargo test -p gridcraft-engine --test it coauthor_replay::"
            ));
            if failures.len() >= 5 {
                break;
            }
        }
    }
    // The checked-in fixtures, as files: the inputs the browser runs read.
    for path in fixture_paths() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match Sequence::from_text(&text) {
            Ok(seq) => {
                if let Err(e) = replay_two(&name, &seq, &mut tally) {
                    failures.push(e);
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{} replay failure(s)\n\n{}", failures.len(), failures.join("\n\n"));
    // A generator whose steps mostly fail would prove little; it also has to change structure.
    let error_share = tally.errors as f64 / tally.steps.max(1) as f64;
    let structural_share = tally.structural as f64 / tally.steps.max(1) as f64;
    eprintln!(
        "coauthor replay: {} sequences, {} steps, {:.1} % errors, {:.1} % structural",
        seeds.len(),
        tally.steps,
        error_share * 100.0,
        structural_share * 100.0
    );
    assert!(error_share < 0.10, "{:.1} % of steps failed", error_share * 100.0);
    assert!((0.08..0.25).contains(&structural_share), "{:.1} % structural steps", structural_share * 100.0);
}

// ---------------------------------------------------------------- hand-written sequences

enum Expect {
    Value(Value),
    Text(&'static str),
    Formula(&'static str),
    Empty,
    Style(fn(&Style) -> bool, &'static str),
    SheetNames(&'static [&'static str]),
}

struct Hand {
    name: &'static str,
    steps: Vec<Step>,
    /// After this many steps, on this sheet and cell, expect this.
    checks: Vec<(usize, &'static str, &'static str, Expect)>,
}

fn set(sheet: &str, cell: &str, input: &str) -> Step {
    Step::new("cell.set", json!({"sheet": sheet, "cell": cell, "input": input}))
}

fn num(n: f64) -> Expect {
    Expect::Value(Value::Number(n))
}

fn hand_written() -> Vec<Hand> {
    vec![
        Hand {
            name: "arithmetic_across_sheets",
            steps: vec![
                set("Sheet1", "A1", "2"),
                set("Sheet1", "A2", "3"),
                set("Sheet1", "A3", "=A1*A2"),
                Step::new("home.insertSheet", json!({"name": "Data", "before": 1})),
                set("Data", "B1", "=Sheet1!A3+1"),
                set("Sheet1", "A1", "4"),
            ],
            checks: vec![
                (5, "Data", "B1", num(7.0)),
                (6, "Sheet1", "A3", num(12.0)),
                (6, "Data", "B1", num(13.0)),
                (6, "Data", "B1", Expect::Formula("Sheet1!A3+1")),
                (6, "", "", Expect::SheetNames(&["Sheet1", "Data"])),
            ],
        },
        Hand {
            name: "insert_rows_moves_references",
            steps: vec![
                set("Sheet1", "A1", "1"),
                set("Sheet1", "A2", "2"),
                set("Sheet1", "A3", "=SUM(A1:A2)"),
                Step::new("home.insertRows", json!({"sheet": "Sheet1", "rows": "2:2"})),
                set("Sheet1", "A2", "10"),
                Step::new("home.deleteRows", json!({"sheet": "Sheet1", "rows": "1:1"})),
            ],
            checks: vec![
                (4, "Sheet1", "A4", Expect::Formula("SUM(A1:A3)")),
                (4, "Sheet1", "A4", num(3.0)),
                (4, "Sheet1", "A2", Expect::Empty),
                (5, "Sheet1", "A4", num(13.0)),
                (6, "Sheet1", "A3", Expect::Formula("SUM(A1:A2)")),
                (6, "Sheet1", "A3", num(12.0)),
                (6, "Sheet1", "A1", num(10.0)),
            ],
        },
        Hand {
            name: "deleted_column_gives_ref_errors",
            steps: vec![
                set("Sheet1", "B1", "5"),
                set("Sheet1", "C1", "=B1*2"),
                set("Sheet1", "D1", "=C1+1"),
                Step::new("home.insertColumns", json!({"sheet": "Sheet1", "cols": "A:A"})),
                Step::new("home.deleteColumns", json!({"sheet": "Sheet1", "cols": "C:C"})),
            ],
            checks: vec![
                (3, "Sheet1", "D1", num(11.0)),
                (4, "Sheet1", "E1", Expect::Formula("D1+1")),
                (4, "Sheet1", "E1", num(11.0)),
                (5, "Sheet1", "C1", Expect::Formula("#REF!*2")),
                (5, "Sheet1", "C1", Expect::Value(Value::Error(CellError::Ref))),
                (5, "Sheet1", "D1", Expect::Value(Value::Error(CellError::Ref))),
            ],
        },
        Hand {
            name: "renamed_then_deleted_sheet",
            steps: vec![
                Step::new("home.insertSheet", json!({"name": "Q1", "before": 1})),
                set("Q1", "A1", "4"),
                set("Sheet1", "A1", "=Q1!A1+1"),
                Step::new("sheet.rename", json!({"sheet": "Q1", "name": "Q 1 Plan"})),
                set("Q 1 Plan", "A1", "40"),
                Step::new("home.deleteSheet", json!({"sheet": "Q 1 Plan"})),
            ],
            checks: vec![
                (3, "Sheet1", "A1", num(5.0)),
                (4, "Sheet1", "A1", Expect::Formula("'Q 1 Plan'!A1+1")),
                (5, "Sheet1", "A1", num(41.0)),
                (6, "Sheet1", "A1", Expect::Value(Value::Error(CellError::Ref))),
                (6, "", "", Expect::SheetNames(&["Sheet1"])),
            ],
        },
        Hand {
            name: "formats_outlive_cleared_content",
            steps: vec![
                set("Sheet1", "A1", "0.5"),
                Step::new("home.numberFormat", json!({"sheet": "Sheet1", "range": "A1:B2", "format": "Percentage"})),
                Step::new("home.bold", json!({"sheet": "Sheet1", "range": "A1", "on": true})),
                Step::new("home.fillColor", json!({"sheet": "Sheet1", "range": "A1", "color": "#FFFF00"})),
                Step::new("home.borders", json!({"sheet": "Sheet1", "range": "A1:B2", "preset": "all"})),
                Step::new("home.alignCenter", json!({"sheet": "Sheet1", "range": "A1:B2", "on": true})),
                Step::new("edit.clearContents", json!({"sheet": "Sheet1", "range": "A1:B2"})),
            ],
            checks: vec![
                (2, "Sheet1", "A1", num(0.5)),
                (7, "Sheet1", "A1", Expect::Empty),
                (7, "Sheet1", "A1", Expect::Style(|s| s.num_fmt.as_str() == "0.00%", "number format 0.00%")),
                (7, "Sheet1", "A1", Expect::Style(|s| s.font.bold, "bold")),
                (7, "Sheet1", "A1", Expect::Style(|s| s.fill == Fill::solid(Color::Rgb(0xFFFF00)), "yellow fill")),
                (
                    7,
                    "Sheet1",
                    "A1",
                    Expect::Style(|s| s.borders.top.style == BorderStyle::Thin && s.borders.right.style == BorderStyle::Thin, "thin box"),
                ),
                (7, "Sheet1", "B2", Expect::Style(|s| s.align.h == HAlign::Center && !s.font.bold, "centred, not bold")),
            ],
        },
        Hand {
            name: "text_lookup_and_logic",
            steps: vec![
                set("Sheet1", "A1", "  hello world "),
                set("Sheet1", "B1", "=UPPER(TRIM(A1))"),
                set("Sheet1", "C1", "=LEN(B1)"),
                set("Sheet1", "D1", "=IF(C1>10,\"long\",\"short\")"),
                set("Sheet1", "G1", "1"),
                set("Sheet1", "H1", "a"),
                set("Sheet1", "G2", "2"),
                set("Sheet1", "H2", "b"),
                set("Sheet1", "E1", "=VLOOKUP(2,G1:H2,2,FALSE)"),
                Step::new("home.insertColumns", json!({"sheet": "Sheet1", "cols": "A:A"})),
                set("Sheet1", "H2", "3"),
            ],
            checks: vec![
                (4, "Sheet1", "B1", Expect::Text("HELLO WORLD")),
                (4, "Sheet1", "C1", num(11.0)),
                (4, "Sheet1", "D1", Expect::Text("long")),
                (9, "Sheet1", "E1", Expect::Text("b")),
                (10, "Sheet1", "F1", Expect::Formula("VLOOKUP(2,H1:I2,2,FALSE)")),
                (10, "Sheet1", "F1", Expect::Text("b")),
                (10, "Sheet1", "A1", Expect::Empty),
                (11, "Sheet1", "F1", Expect::Value(Value::Error(CellError::NA))),
            ],
        },
    ]
}

fn hand_sequence(i: usize, h: &Hand) -> Sequence {
    Sequence { seed: 9_000 + i as u64, now: DEFAULT_NOW, steps: h.steps.clone() }
}

fn check(s: &Session, sheet: &str, cell: &str, e: &Expect) -> Result<(), String> {
    let wb = &s.doc().map_err(|e| e.to_string())?.wb;
    if let Expect::SheetNames(names) = e {
        let got: Vec<&str> = wb.sheets.iter().map(|s| s.name.as_str()).collect();
        return if got == *names { Ok(()) } else { Err(format!("sheets {got:?}, expected {names:?}")) };
    }
    let i = wb.sheet_index(sheet).ok_or(format!("no sheet `{sheet}`"))?;
    let sh = wb.sheet(i).ok_or("no sheet")?;
    let at = CellRef::parse(cell).ok_or("bad cell")?;
    let v = sh.value(at);
    let ok = match e {
        Expect::Value(x) => v == *x,
        Expect::Text(t) => v == Value::Text((*t).into()),
        Expect::Empty => v == Value::Empty && sh.cell(at).is_none_or(|c| c.formula.is_none()),
        Expect::Formula(f) => sh.cell(at).and_then(|c| c.formula.as_ref()).is_some_and(|x| x.text == *f),
        Expect::Style(f, _) => f(wb.styles.get(sh.style_id(at))),
        Expect::SheetNames(_) => true,
    };
    if ok {
        return Ok(());
    }
    let formula = sh.cell(at).and_then(|c| c.formula.as_ref()).map(|f| f.text.clone());
    let what = match e {
        Expect::Value(x) => format!("value {x:?}"),
        Expect::Text(t) => format!("text {t:?}"),
        Expect::Empty => "empty".into(),
        Expect::Formula(f) => format!("formula {f:?}"),
        Expect::Style(_, d) => format!("style: {d}"),
        Expect::SheetNames(_) => String::new(),
    };
    Err(format!("{sheet}!{cell}: expected {what}, got value {v:?}, formula {formula:?}"))
}

#[test]
fn replay_matches_hand_written_expectations() {
    let mut failures = Vec::new();
    for (i, h) in hand_written().iter().enumerate() {
        let seq = hand_sequence(i, h);
        // Both engines agree on every step…
        if let Err(e) = replay_two(h.name, &seq, &mut Tally::default()) {
            failures.push(e);
            continue;
        }
        // …and are right: the checks after each prefix of the sequence, in both engines.
        let mut engines = [fresh_session(&seq), fresh_session(&seq)];
        for (n, step) in seq.steps.iter().enumerate() {
            for s in engines.iter_mut() {
                if let Err(e) = apply(s, step) {
                    failures.push(format!("{}: step {n} `{}` failed: {e}", h.name, step.id));
                }
            }
            for (after, sheet, cell, e) in &h.checks {
                if *after == n + 1 {
                    for (k, s) in engines.iter().enumerate() {
                        if let Err(msg) = check(s, sheet, cell, e) {
                            failures.push(format!("{} (engine {k}), after step {after}: {msg}", h.name));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------- fixtures

fn fixture_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// The fixtures as they should be: generated seeds 1..=20 and the hand-written sequences.
fn expected_fixtures() -> Vec<(String, Sequence)> {
    let mut out: Vec<(String, Sequence)> = (1..=FIXTURES).map(|seed| (format!("random-{seed:03}.json"), generate(seed, STEPS))).collect();
    for (i, h) in hand_written().iter().enumerate() {
        out.push((format!("hand-{}-{}.json", i + 1, h.name.replace('_', "-")), hand_sequence(i, h)));
    }
    out
}

#[test]
fn fixtures_round_trip_as_json() {
    let expected = expected_fixtures();
    if std::env::var_os("GRIDCRAFT_COAUTHOR_WRITE_FIXTURES").is_some() {
        for (name, seq) in &expected {
            std::fs::write(fixtures_dir().join(name), seq.to_text()).expect("fixture written");
        }
    }
    let paths = fixture_paths();
    let names: Vec<String> = paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
    for (name, _) in &expected {
        assert!(names.contains(name), "missing fixture {name} (GRIDCRAFT_COAUTHOR_WRITE_FIXTURES=1 writes them)");
    }
    assert!(names.iter().filter(|n| n.starts_with("random-")).count() >= 20, "20 generated fixtures are checked in");
    for path in &paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Git may check text out with CRLF line ends; the JSON is the same either way.
        let text = std::fs::read_to_string(path).expect("fixture readable").replace("\r\n", "\n");
        let seq = Sequence::from_text(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(seq.to_text(), text, "{name} writes back byte for byte");
        assert_eq!(Sequence::from_json(&seq.to_json()).as_ref(), Ok(&seq), "{name} survives a JSON value round trip");
        if let Some((_, want)) = expected.iter().find(|(n, _)| *n == name) {
            assert_eq!(&seq, want, "{name} matches what the generator or the hand-written list makes now");
        }
        for step in &seq.steps {
            assert!(SHARED_COMMANDS.contains(&step.id.as_str()), "{name}: `{}` is not a shared command", step.id);
        }
    }
    // The JSON form rejects what it doesn't know, so a typo can't pass as an empty sequence.
    for bad in [
        r#"{"seed": 1, "now": 1, "steps": [], "extra": 0}"#,
        r#"{"seed": -1, "now": 1, "steps": []}"#,
        r#"{"seed": 1, "steps": []}"#,
        r#"{"seed": 1, "now": 1, "steps": [{"id": "cell.set"}]}"#,
        r#"{"seed": 1, "now": 1, "steps": [{"id": "cell.set", "params": []}]}"#,
    ] {
        assert!(Sequence::from_text(bad).is_err(), "{bad} is refused");
    }
    // Extreme values survive the text form exactly.
    let edge = Sequence { seed: u64::MAX, now: 46_302.123_456_789_01, steps: vec![set("Sheet1", "A1", "=1/3")] };
    assert_eq!(Sequence::from_text(&edge.to_text()).as_ref(), Ok(&edge));
}

// ---------------------------------------------------------------- volatile functions

#[test]
fn volatile_functions_agree_with_fixed_now_and_seed() {
    let steps = vec![
        set("Sheet1", "A1", "=NOW()"),
        set("Sheet1", "A2", "=TODAY()"),
        set("Sheet1", "A3", "=RAND()"),
        set("Sheet1", "A4", "=RANDBETWEEN(1,100)"),
        set("Sheet1", "A5", "=A3*2"),
        set("Sheet1", "B1", "7"),
        Step::new("home.insertRows", json!({"sheet": "Sheet1", "rows": "1:1"})),
        set("Sheet1", "B1", "8"),
        Step::new("home.insertSheet", json!({"name": "Two", "before": 1})),
        set("Two", "A1", "=Sheet1!A4+RAND()"),
    ];
    let seq = Sequence { seed: 42, now: DEFAULT_NOW, steps };
    let (a, b) = replay_two("volatile", &seq, &mut Tally::default()).unwrap_or_else(|e| panic!("{e}"));
    let value = |s: &Session, sheet: usize, a1: &str| s.doc().unwrap().wb.sheet(sheet).unwrap().value(CellRef::parse(a1).unwrap());
    // The clock is the sequence's, exactly.
    assert_eq!(value(&a, 0, "A2"), Value::Number(DEFAULT_NOW));
    assert_eq!(value(&a, 0, "A3"), Value::Number(DEFAULT_NOW.floor()));
    // RAND draws in [0, 1), RANDBETWEEN whole numbers in range, the same in both engines.
    let r = value(&a, 0, "A4").as_f64().unwrap();
    assert!((0.0..1.0).contains(&r), "RAND gave {r}");
    let k = value(&a, 0, "A5").as_f64().unwrap();
    assert!(k.fract() == 0.0 && (1.0..=100.0).contains(&k), "RANDBETWEEN gave {k}");
    assert_eq!(value(&a, 0, "A6"), Value::Number(r * 2.0));
    assert_eq!(value(&a, 1, "A1"), value(&b, 1, "A1"));
    // The seed matters: another one draws other numbers (and the entered content is the same).
    let other = Sequence { seed: 43, ..seq.clone() };
    let (c, _) = replay_two("volatile, seed 43", &other, &mut Tally::default()).unwrap_or_else(|e| panic!("{e}"));
    let (fa, fc) = (fingerprint(&a, DocId(0)), fingerprint(&c, DocId(0)));
    assert_eq!(fa.entered, fc.entered);
    assert_ne!(fa.computed, fc.computed, "seed 43 draws the same numbers as seed 42");
    assert_ne!(value(&c, 0, "A4"), value(&a, 0, "A4"));
    // So does the clock.
    let later = Sequence { now: DEFAULT_NOW + 1.25, ..seq };
    let (d, _) = replay_two("volatile, a day later", &later, &mut Tally::default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(value(&d, 0, "A2"), Value::Number(DEFAULT_NOW + 1.25));
    assert_eq!(value(&d, 0, "A4"), value(&a, 0, "A4"), "the clock doesn't move the draws");
}
