//! Release-only timings for rolling back to an agreed workbook and re-applying pending changes.

use std::sync::Arc;
use std::time::Instant;

use crate::sequences::{
    TIMING_CELL_COUNTS, TIMING_PENDING_COUNTS, TIMING_REPETITIONS, TimingStats, apply, rerun_pending, timing_document, timing_pending_commands,
    timing_stats, timing_workbook,
};

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

#[allow(clippy::disallowed_methods)] // native-only test; the brief requires std::time::Instant
fn measure(agreed: &gridcraft_engine::DocState, steps: &[crate::sequences::Step]) -> Result<f64, String> {
    let start = Instant::now();
    let rerun = rerun_pending(agreed, steps);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1_000.0;
    std::hint::black_box(rerun)?;
    Ok(elapsed_ms)
}

#[test]
#[ignore = "release-only rollback and re-run timing proof"]
fn rollback_and_rerun_timings_native() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("timings must be run with cargo test --release".into());
    }
    let pending = timing_pending_commands();
    let mut rows: Vec<(usize, usize, TimingStats)> = Vec::new();
    for cells in TIMING_CELL_COUNTS {
        let agreed = timing_document(cells)?;
        for command_count in TIMING_PENDING_COUNTS {
            let steps = pending.get(..command_count).ok_or_else(|| format!("only {} pending commands were generated", pending.len()))?;
            let _warm_up_ms = measure(&agreed, steps)?;
            let mut samples_ms = Vec::with_capacity(TIMING_REPETITIONS);
            for _ in 0..TIMING_REPETITIONS {
                samples_ms.push(measure(&agreed, steps)?);
            }
            rows.push((cells, command_count, timing_stats(samples_ms)?));
        }
    }

    println!("platform cells pending p50_ms p95_ms max_ms");
    for (cells, pending, stats) in &rows {
        println!("native {cells} {pending} {:.3} {:.3} {:.3}", stats.p50_ms, stats.p95_ms, stats.max_ms);
    }
    let measured = rows
        .iter()
        .find(|(cells, pending, _)| *cells == 100_000 && *pending == 10)
        .map(|(_, _, stats)| stats.p95_ms)
        .ok_or("the 100,000-cell/10-command case was not measured")?;
    if measured > 50.0 {
        return Err(format!("native 100,000-cell/10-command p95 {measured:.3} ms exceeds 50 ms"));
    }
    Ok(())
}

#[test]
#[ignore = "profiles a native threshold miss by clone, command application and final recalc"]
#[allow(clippy::disallowed_methods)] // native-only test; the brief requires std::time::Instant
fn profile_rollback_and_rerun_native_100k_10() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("timings must be run with cargo test --release".into());
    }
    let agreed = timing_document(100_000)?;
    let pending = timing_pending_commands();
    let steps = pending.get(..10).ok_or("fifty pending commands were not generated")?;
    let mut clone_ms = Vec::with_capacity(TIMING_REPETITIONS);
    let mut apply_ms = Vec::with_capacity(TIMING_REPETITIONS);
    let mut recalc_ms = Vec::with_capacity(TIMING_REPETITIONS);
    for repetition in 0..=TIMING_REPETITIONS {
        let start = Instant::now();
        let mut session = gridcraft_engine::Session::new();
        session.add_document(agreed.clone());
        let cloned_ms = start.elapsed().as_secs_f64() * 1_000.0;

        let start = Instant::now();
        for step in steps {
            apply(&mut session, step)?;
        }
        let applied_ms = start.elapsed().as_secs_f64() * 1_000.0;

        let start = Instant::now();
        let document = session.doc_mut().map_err(|error| error.to_string())?;
        let wb = Arc::make_mut(&mut document.wb);
        document.calc.recalc_all(wb);
        let rebuilt_ms = start.elapsed().as_secs_f64() * 1_000.0;
        std::hint::black_box(session);

        if repetition != 0 {
            clone_ms.push(cloned_ms);
            apply_ms.push(applied_ms);
            recalc_ms.push(rebuilt_ms);
        }
    }

    println!("phase p50_ms p95_ms max_ms");
    for (phase, stats) in [("clone", timing_stats(clone_ms)?), ("apply_10", timing_stats(apply_ms)?), ("final_recalc", timing_stats(recalc_ms)?)] {
        println!("{phase} {:.3} {:.3} {:.3}", stats.p50_ms, stats.p95_ms, stats.max_ms);
    }
    Ok(())
}
