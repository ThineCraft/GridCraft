//! Timing for ordinary (not live) formula evaluation: operator- and call-heavy formulas over
//! 200,000 lambda calls. Ignored by default; run it in release to compare two builds:
//! `cargo test -p gridcraft-calc --release --test evaluation_timing -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use gridcraft_core::{CellRef, Value};
use gridcraft_model::Workbook;

const FORMULAS: [(&str, &str); 3] = [
    ("ABS(x)+1", "=SUM(MAP(SEQUENCE(200000),LAMBDA(x,ABS(x)+1)))"),
    ("(x+1)*2-x/3", "=SUM(MAP(SEQUENCE(200000),LAMBDA(x,(x+1)*2-x/3)))"),
    ("LEN(\"ab\")+VALUE(\"3\")", "=SUM(MAP(SEQUENCE(200000),LAMBDA(x,LEN(\"ab\")+VALUE(\"3\"))))"),
];

fn best_of(runs: usize, formula: &str, workbook: &Workbook) -> (Duration, Value) {
    let mut best = Duration::MAX;
    let mut value = Value::Empty;
    for _ in 0..runs {
        let started = Instant::now();
        value = gridcraft_calc::evaluate(workbook, 0, CellRef::new(0, 0), formula);
        best = best.min(started.elapsed());
    }
    (best, value)
}

#[test]
#[ignore = "timing; run in release with --ignored --nocapture"]
fn operator_and_call_heavy_evaluation() {
    let workbook = Workbook::new();
    for round in 1..=2 {
        for (name, formula) in FORMULAS {
            let (best, value) = best_of(7, formula, &workbook);
            assert!(matches!(value, Value::Number(_)), "{name} evaluates to a number, got {value:?}");
            println!("round {round}: {name}: best of 7 = {:.1} ms", best.as_secs_f64() * 1000.0);
        }
    }
}
