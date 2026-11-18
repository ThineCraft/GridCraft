use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, LiveEditor, Mode};
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::coauthor::GridLiveEditor;
use gridcraft_engine::fingerprint::{DocId, fingerprint};
use gridcraft_engine::{EngineError, UiRequest};
use serde_json::json;
use std::sync::Arc;

use crate::live::{SimulatedServer, prepare, remote_entry, select, session_with_sheets};

fn active_view(editor: &GridLiveEditor) -> (String, CellRef) {
    let document = editor.session().doc().expect("document exists");
    let name = document.wb.active().expect("active sheet exists").name.clone();
    (name, document.selection.active)
}

#[test]
fn snapshot_restores_shared_state_not_personal_view() {
    let mut session = session_with_sheets(&["Data", "View"]);
    select(&mut session, "View", "C3");
    {
        let document = session.doc_mut().expect("document exists");
        document.calc.fixed_now = Some(12_345.0);
        document.calc.set_rand_seed(77);
    }
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let before_entered = fingerprint(editor.session(), DocId(0)).entered;
    let before_ids = editor.ids().clone();
    let before_revision = editor.session().doc().expect("document exists").revision;
    let snapshot = editor.snapshot();

    let mut next = 0;
    let change = prepare(&mut editor, 1, "home.insertSheet", json!({"name":"Remote","before":0}), &mut next);
    editor.apply(&remote_entry(&change.payload, 1)).expect("remote insert applies");
    select(editor.session_mut(), "Data", "F6");
    editor.session_mut().ui_requests.push(UiRequest::Message("stale".into()));

    editor.restore(&snapshot);

    let document = editor.session().doc().expect("document exists");
    assert_eq!(fingerprint(editor.session(), DocId(0)).entered, before_entered, "workbook content restores");
    assert_eq!(editor.ids(), &before_ids, "identity tables restore");
    assert_eq!(document.revision, before_revision, "revision restores");
    assert_eq!(document.calc.fixed_now, Some(12_345.0), "calculation state restores");
    assert_eq!(active_view(&editor), ("Data".into(), CellRef::new(5, 5)), "personal view does not roll back");
    assert!(!document.is_dirty(), "restoring personal view does not turn a clean shared snapshot dirty");
    assert!(editor.session().ui_requests.is_empty(), "queued requests from the abandoned state are cleared");
}

#[test]
fn remote_insert_and_delete_keep_the_local_view() {
    let mut session = session_with_sheets(&["Data", "View", "Tail"]);
    select(&mut session, "Tail", "H8");
    select(&mut session, "View", "D4");
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let mut next = 0;

    let insert = prepare(&mut editor, 2, "home.insertSheet", json!({"name":"Remote","before":0}), &mut next);
    assert_eq!(insert.mode, Mode::ShowAtOnce);
    editor.apply(&remote_entry(&insert.payload, 1)).expect("remote insert applies");
    assert_eq!(active_view(&editor), ("View".into(), CellRef::new(3, 3)));

    let delete = prepare(&mut editor, 3, "home.deleteSheet", json!({"sheet":"Data"}), &mut next);
    editor.apply(&remote_entry(&delete.payload, 2)).expect("remote delete applies");
    assert_eq!(active_view(&editor), ("View".into(), CellRef::new(3, 3)));

    select(editor.session_mut(), "Tail", "A1");
    assert_eq!(active_view(&editor), ("Tail".into(), CellRef::new(0, 0)), "the sheet's remembered selection remains usable");
}

#[test]
fn deleted_active_sheet_uses_nearest_survivor() {
    let mut session = session_with_sheets(&["Before", "Active", "After"]);
    select(&mut session, "Before", "B2");
    select(&mut session, "After", "F6");
    select(&mut session, "Active", "D4");
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let mut next = 0;
    let delete = prepare(&mut editor, 4, "home.deleteSheet", json!({"sheet":"Active"}), &mut next);

    editor.apply(&remote_entry(&delete.payload, 1)).expect("active sheet deletion applies");

    assert_eq!(active_view(&editor), ("After".into(), CellRef::new(5, 5)), "the next sheet and its remembered selection win");

    let mut session = session_with_sheets(&["Before", "Last"]);
    select(&mut session, "Before", "B2");
    select(&mut session, "Last", "D4");
    let mut editor = GridLiveEditor::new(session).expect("second editor builds");
    let delete = prepare(&mut editor, 5, "home.deleteSheet", json!({"sheet":"Last"}), &mut next);
    editor.apply(&remote_entry(&delete.payload, 1)).expect("last sheet deletion applies");
    assert_eq!(active_view(&editor), ("Before".into(), CellRef::new(1, 1)), "the preceding sheet is the end fallback");
}

#[test]
fn internal_engine_error_is_a_live_crash() {
    let result = GridLiveEditor::engine_error_outcome(EngineError::Internal("cell.set".into(), "private detail".into()), false);
    assert_eq!(result, Err(Crash));
}

#[test]
fn note_applying_tracks_the_seq() {
    let mut editor = GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds");
    editor.note_applying(Some(Seq(1)));
    assert_eq!(editor.applying_seq(), Some(Seq(1)));

    let crashed_at = editor.applying_seq();
    let (mut server, mut client) = SimulatedServer::start(&mut editor, crashed_at, 1);
    let mut next = 0;
    let change = prepare(&mut editor, 6, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"not applied"}), &mut next);
    server.queue_remote(&mut client, &mut editor, &change.payload);
    let step = client.base_ready(&mut editor, 10).expect("base becomes ready up to the crash");

    assert!(step.events.iter().any(|event| matches!(event, collab_live::client::Event::Crashed { seq: Seq(1) })));
    assert_eq!(editor.applying_seq(), Some(Seq(1)), "the reloaded client keeps the crash note");
    let value = editor.session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(0, 0));
    assert_eq!(value, Value::Empty, "the crashed entry is not applied again");
}

#[test]
fn large_workbook_structural_prepare_waits_for_server() {
    let mut session = session_with_sheets(&["Sheet1"]);
    {
        let document = session.doc_mut().expect("document exists");
        let workbook = Arc::make_mut(&mut document.wb);
        let sheet = workbook.sheet_mut(0).expect("sheet exists");
        for row in 0..100_000 {
            sheet.set_value(CellRef::new(row, 0), Value::Number(1.0));
        }
    }
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let mut next = 0;

    let structural = prepare(&mut editor, 7, "home.insertRows", json!({"sheet":"Sheet1","rows":"1","count":1}), &mut next);
    let rename = prepare(&mut editor, 8, "sheet.rename", json!({"sheet":"Sheet1","name":"Renamed"}), &mut next);
    let content = prepare(&mut editor, 9, "cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"x"}), &mut next);

    assert_eq!(structural.mode, Mode::WaitForServer);
    assert_eq!(rename.mode, Mode::WaitForServer);
    assert_eq!(content.mode, Mode::ShowAtOnce);
    assert!(
        matches!(GridLiveEditor::engine_error_outcome(EngineError::Other("no".into()), false), Ok(outcome) if matches!(outcome.effect, Effect::NoEffect { .. }))
    );
}
