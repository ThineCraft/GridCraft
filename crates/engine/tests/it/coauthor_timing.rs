//! Release-only timings for rolling back to an agreed workbook and re-applying pending changes.
//!
//! The headline figure is clone + apply: each command recalculates as it is committed, which is
//! what the co-authoring apply loop does. Clone + apply + one forced `recalc_all` is reported
//! beside it as an upper bound, and an untimed check proves that the forced pass changes nothing.
//!
//! What the results support, and what they do not:
//! - The workbook (`timing_workbook`) is a lower bound on formula cost, so the content-edit
//!   timings hold for its shape only.
//! - Structural commands are at or above the 50 ms budget alone, not uniformly above it: one row
//!   or column insert or delete costs 43-60 ms p50 at 100,000 cells, and any batch of 10 that
//!   contains one misses 50 ms, at p50 as well as p95 (53-59 ms p50).
//! - The WebAssembly counterpart (`tests/wasm_replay.rs`) runs under Node, not in a browser
//!   engine; its 150 ms pass is Node-hosted and provisional until a browser run.

use std::time::Instant;

use crate::sequences::{
    Step, TIMING_REPETITIONS, check_rerun, headline_p95, heavier_timing_document, is_structural, kind_table, rerun_pending, time_all_workloads,
    time_command_kinds, time_workload, timing_command_kinds, timing_document, timing_stats, timing_table, timing_workbook, timing_workloads,
};
use serde_json::json;

#[test]
fn timing_workbook_has_the_requested_shape() {
    let wb = timing_workbook(10_000).expect("the smallest proof workbook is valid");
    let cells = wb.sheets.iter().map(|sheet| sheet.cells.len()).sum::<usize>();
    let formulas = wb.sheets.iter().flat_map(|sheet| sheet.cells.iter()).filter(|(_, cell)| cell.formula.is_some()).count();
    let formula_texts = wb
        .sheets
        .iter()
        .flat_map(|sheet| sheet.cells.iter())
        .filter_map(|(_, cell)| cell.formula.as_ref().map(|formula| formula.text.as_str()))
        .collect::<Vec<_>>();

    assert_eq!(cells, 10_000);
    assert_eq!(formulas, 2_000);
    assert!(formula_texts.iter().any(|formula| formula.contains("SUM(B:B)")));
    assert!(formula_texts.iter().any(|formula| formula.contains("Data!")));
    assert!(formula_texts.iter().any(|formula| formula.contains("Sheet1!")));
}

#[test]
fn timing_percentiles_use_nearest_rank() {
    let stats = timing_stats((1..=30).map(|sample| sample as f64).collect()).expect("thirty finite samples are valid");
    assert_eq!((stats.p50_ms, stats.p95_ms, stats.max_ms), (15.0, 29.0, 30.0));
}

#[test]
fn timing_workloads_have_the_stated_structural_mix() {
    let workloads = timing_workloads();
    let shape =
        workloads.iter().map(|(name, steps)| (*name, steps.len(), steps.iter().filter(|step| is_structural(step)).count())).collect::<Vec<_>>();
    assert_eq!(shape, [("mixed", 1, 0), ("mixed", 10, 1), ("mixed", 50, 7), ("content-only", 10, 0), ("one-structural", 10, 1)]);
}

/// The untimed checks the timing runs make, here on the smallest workbook: every workload changes
/// the entered digest, and a further `recalc_all` after the per-command recalculation changes
/// neither digest.
#[test]
fn rerun_without_a_final_recalc_is_fully_calculated() {
    let agreed = timing_document(10_000).expect("the smallest proof workbook is valid");
    for (workload, steps) in timing_workloads() {
        let rerun = rerun_pending(&agreed, &steps).expect("the pending commands apply");
        check_rerun(&agreed, rerun, &steps).unwrap_or_else(|error| panic!("{workload}: {error}"));
    }
    for step in timing_command_kinds() {
        let steps = std::slice::from_ref(&step);
        let rerun = rerun_pending(&agreed, steps).expect("the command applies");
        check_rerun(&agreed, rerun, steps).unwrap_or_else(|error| panic!("{}: {error}", step.id));
    }
}

/// Milliseconds since the first call, from `std::time::Instant` as the brief requires.
#[allow(clippy::disallowed_methods)] // native-only test; reads the clock off wasm
fn native_clock() -> impl Fn() -> f64 {
    let origin = Instant::now();
    move || origin.elapsed().as_secs_f64() * 1_000.0
}

#[test]
#[ignore = "release-only rollback and re-run timing proof"]
fn rollback_and_rerun_timings_native() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("timings must be run with cargo test --release".into());
    }
    let clock = native_clock();
    let rows = time_all_workloads(&clock)?;
    let kinds = time_command_kinds(&clock, 100_000)?;
    for line in timing_table("native", &rows) {
        println!("{line}");
    }
    for line in kind_table("native", 100_000, &kinds) {
        println!("{line}");
    }
    println!("repetitions {TIMING_REPETITIONS} after one excluded warm-up");
    let measured = headline_p95(&rows)?;
    println!("headline native mixed 100000 cells 10 pending clone+apply p95 {measured:.3} ms (threshold 50 ms)");
    if measured > 50.0 {
        return Err(format!("native 100,000-cell/10-command clone+apply p95 {measured:.3} ms exceeds 50 ms"));
    }
    Ok(())
}

#[test]
#[ignore = "release-only heavier-formula rollback and re-run report"]
fn heavier_formula_rerun_report() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("timings must be run with cargo test --release".into());
    }
    let clock = native_clock();
    let agreed = heavier_timing_document(100_000)?;
    let content =
        (2..12).map(|row| Step::new("cell.set", json!({"sheet":"Sheet1","cell":format!("B{row}"),"input":row.to_string()}))).collect::<Vec<_>>();
    let structural = vec![Step::new("home.insertRows", json!({"sheet":"Sheet1","rows":"10:10"}))];
    let rows = vec![
        time_workload(&clock, &agreed, 100_000, "heavier-content-only", &content)?,
        time_workload(&clock, &agreed, 100_000, "heavier-one-structural", &structural)?,
    ];
    for line in timing_table("native", &rows) {
        println!("{line}");
    }
    println!("repetitions {TIMING_REPETITIONS} after one excluded warm-up; report only, no wall-clock gate");
    Ok(())
}
